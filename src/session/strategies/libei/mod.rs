//! libei/EIS Strategy: Flatpak-Compatible wlroots Input
//!
//! This module implements input injection using the libei (Emulated Input) protocol via
//! Portal RemoteDesktop.ConnectToEIS(), providing Flatpak-compatible wlroots support.
//!
//! # Overview
//!
//! The libei strategy uses the Portal RemoteDesktop interface to obtain an EIS (Emulated
//! Input Server) socket, then communicates via the EI protocol using the `reis` crate.
//!
//! # Architecture
//!
//! ```text
//! lamco-rdp-server (Flatpak or native)
//!   | D-Bus
//! Portal RemoteDesktop
//!   +- CreateSession()
//!   +- SelectDevices(keyboard, pointer, touchscreen)
//!   +- Start() -> user approves if needed
//!   +- ConnectToEIS() -> Unix socket FD
//!       |
//! EIS Protocol (via reis crate)
//!   +- Handshake (version, capabilities)
//!   +- Seat discovery
//!   +- Device creation (keyboard, pointer, touchscreen)
//!   +- Event streaming (key, motion, button, scroll, touch)
//!       |
//! Portal backend (xdg-desktop-portal-wlr, hyprland, etc.)
//!   +- Compositor protocols (zwp_virtual_keyboard, zwlr_virtual_pointer)
//! ```
//!
//! # Compatibility
//!
//! **Works with:**
//! - xdg-desktop-portal-wlr with PR #359 (InputCapture + RemoteDesktop/ConnectToEIS)
//! - xdg-desktop-portal-hyprland with ConnectToEIS support
//! - Any portal backend implementing RemoteDesktop v2+ with ConnectToEIS
//!
//! **Flatpak compatible:** Yes (Portal provides socket FD across sandbox boundary)

use std::{collections::HashMap, os::unix::net::UnixStream, sync::Arc};

use anyhow::{Context as AnyhowContext, Result};
use ashpd::desktop::{
    PersistMode,
    remote_desktop::{DeviceType, RemoteDesktop},
};
use async_trait::async_trait;
use futures::stream::StreamExt;
use reis::{PendingRequestResult, ei, tokio::EiEventStream};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, error, info, warn};

use super::eis_common::{self, DeviceData, EisDevices};
use crate::{
    health::{HealthEvent, HealthReporter},
    session::{
        Tokens,
        strategy::{PipeWireAccess, SessionHandle, SessionStrategy, SessionType, StreamInfo},
    },
};

/// libei/EIS strategy implementation
///
/// Provides input injection via Portal RemoteDesktop + EIS protocol.
pub struct LibeiStrategy {
    #[expect(dead_code, reason = "retained for future EIS session recovery")]
    portal_manager: Option<Arc<lamco_portal::PortalManager>>,
    token_manager: Option<Arc<Tokens>>,
}

impl LibeiStrategy {
    pub fn new(
        portal_manager: Option<Arc<lamco_portal::PortalManager>>,
        token_manager: Option<Arc<Tokens>>,
    ) -> Self {
        Self {
            portal_manager,
            token_manager,
        }
    }

    pub async fn is_available() -> bool {
        match RemoteDesktop::new().await {
            Ok(_rd) => {
                debug!("[libei] Portal RemoteDesktop proxy created successfully");
                true
            }
            Err(e) => {
                debug!("[libei] Portal RemoteDesktop not available: {}", e);
                false
            }
        }
    }

    /// Create the session returning the CONCRETE handle type.
    ///
    /// Composing strategies (kwin-virtual) need the concrete
    /// `LibeiSessionHandleImpl` to delegate input calls without an extra
    /// trait-object hop. The trait's `create_session` delegates here and
    /// coerces the result.
    pub async fn create_session_concrete(&self) -> Result<Arc<LibeiSessionHandleImpl>> {
        info!("libei: Creating session with Portal RemoteDesktop + EIS");

        let remote_desktop = RemoteDesktop::new()
            .await
            .context("Failed to create RemoteDesktop proxy")?;

        let session = remote_desktop
            .create_session(ashpd::desktop::CreateSessionOptions::default())
            .await
            .context("Failed to create RemoteDesktop session")?;

        let session_tag = format!("libei-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        info!(
            session_tag = %session_tag,
            "[libei] Portal RemoteDesktop session created"
        );

        let restore_token = if let Some(ref tm) = self.token_manager {
            match tm.load_token("libei-default").await {
                Ok(Some(token)) => {
                    info!("libei: Loaded restore token ({} chars)", token.len());
                    Some(token)
                }
                Ok(None) => {
                    info!("libei: No restore token found, permission dialog will appear");
                    None
                }
                Err(e) => {
                    warn!("libei: Failed to load restore token: {}", e);
                    None
                }
            }
        } else {
            None
        };

        use ashpd::desktop::remote_desktop::SelectDevicesOptions;
        remote_desktop
            .select_devices(
                &session,
                SelectDevicesOptions::default()
                    .set_devices(
                        DeviceType::Keyboard | DeviceType::Pointer | DeviceType::Touchscreen,
                    )
                    .set_restore_token(restore_token.as_deref())
                    .set_persist_mode(PersistMode::ExplicitlyRevoked),
            )
            .await
            .context("Failed to select input devices")?;

        info!("libei: Selected keyboard, pointer, and touchscreen devices");

        let response = remote_desktop
            .start(
                &session,
                None,
                ashpd::desktop::remote_desktop::StartOptions::default(),
            )
            .await
            .context("Failed to start RemoteDesktop session")?;

        let selected = response.response()?;
        let new_token = selected.restore_token().map(ToString::to_string);

        if let Some(ref token) = new_token {
            info!("libei: Received restore token ({} chars)", token.len());
            if let Some(ref tm) = self.token_manager
                && let Err(e) = tm.save_token("libei-default", token).await
            {
                warn!("libei: Failed to save restore token: {}", e);
            }
        } else {
            debug!("[libei] No restore token in response (portal may not support persistence)");
        }

        info!("libei: Portal session ready (EIS deferred until client connects)");

        let portal_session = Arc::new(RwLock::new(session));

        // Session.Closed watchdog: the portal backend can close the session
        // behind our back (e.g. backend restart), and every EIS handle
        // derived from it then writes into a dead D-Bus session — the
        // client observes input failing later as a TCP RST cascade. Log
        // the closure at ERROR so the root cause is distinguishable from
        // the delayed connection failure it causes.
        {
            let session_for_closed = portal_session.clone();
            let task_tag = session_tag.clone();
            tokio::spawn(async move {
                let session_guard = session_for_closed.read_owned().await;
                let mut closed_stream = match session_guard.receive_closed().await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(
                            session_tag = %task_tag,
                            error = %e,
                            "[libei] Failed to subscribe to Session.Closed signal"
                        );
                        return;
                    }
                };
                info!(
                    session_tag = %task_tag,
                    "[libei] Subscribed to Portal Session.Closed signal"
                );
                match closed_stream.next().await {
                    Some(payload) => {
                        error!(
                            session_tag = %task_tag,
                            payload = ?payload,
                            "[libei] PORTAL SESSION CLOSED by backend — input/EIS path is now dead"
                        );
                    }
                    None => {
                        warn!(
                            session_tag = %task_tag,
                            "[libei] Portal Session.Closed stream ended without a Closed event (D-Bus connection lost?)"
                        );
                    }
                }
            });
        }

        let handle = Arc::new_cyclic(|weak| LibeiSessionHandleImpl {
            portal_session,
            remote_desktop: Arc::new(remote_desktop),
            context: Arc::new(RwLock::new(None)),
            connection: Arc::new(Mutex::new(None)),
            seats: Arc::new(Mutex::new(HashMap::new())),
            devices: Arc::new(EisDevices::new(0)),
            streams: Arc::new(Mutex::new(vec![])),
            health_reporter: std::sync::OnceLock::new(),
            eis_activated: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            activating: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            weak_self: std::sync::OnceLock::from(weak.clone()),
        });

        Ok(handle)
    }
}

impl Default for LibeiStrategy {
    fn default() -> Self {
        Self::new(None, None)
    }
}

#[async_trait]
impl SessionStrategy for LibeiStrategy {
    fn name(&self) -> &'static str {
        "libei"
    }

    fn requires_initial_setup(&self) -> bool {
        true
    }

    fn supports_unattended_restore(&self) -> bool {
        true
    }

    async fn create_session(&self) -> Result<Arc<dyn SessionHandle>> {
        let handle = self.create_session_concrete().await?;
        Ok(handle as Arc<dyn SessionHandle>)
    }

    async fn cleanup(&self, _session: &dyn SessionHandle) -> Result<()> {
        info!("libei: Session cleanup complete");
        Ok(())
    }
}

/// Seat data for EIS seats
#[derive(Default)]
struct SeatData {
    name: Option<String>,
    capabilities: HashMap<String, u64>,
}

/// libei session handle implementation
///
/// Implements SessionHandle trait using event-driven EIS protocol.
/// The context and devices are behind RwLock to allow replacement
/// during EIS session recovery (when the EIS socket dies due to
/// compositor idle timeout).
pub struct LibeiSessionHandleImpl {
    /// Portal session (alive for server lifetime, holds permissions)
    portal_session: Arc<RwLock<ashpd::desktop::Session<RemoteDesktop>>>,
    /// RemoteDesktop proxy for calling ConnectToEIS
    remote_desktop: Arc<RemoteDesktop>,
    /// EIS context — None until activate_input() is called
    context: Arc<RwLock<Option<ei::Context>>>,
    /// EIS connection — None until activated
    connection: Arc<Mutex<Option<ei::Connection>>>,
    /// Seat tracking (populated by event loop after activation)
    seats: Arc<Mutex<HashMap<ei::Seat, SeatData>>>,
    /// Input devices (populated by event loop after activation)
    devices: Arc<EisDevices>,
    /// External video streams (set by server, independent of EIS)
    streams: Arc<Mutex<Vec<StreamInfo>>>,
    health_reporter: std::sync::OnceLock<HealthReporter>,
    /// Whether EIS has been activated (ConnectToEIS called)
    eis_activated: Arc<std::sync::atomic::AtomicBool>,
    /// Prevent concurrent activation
    activating: Arc<std::sync::atomic::AtomicBool>,
    /// Weak self-reference — lets the persistent EIS event-consumer task
    /// call `handle_event` without creating an Arc cycle (the task must not
    /// keep the handle alive if everything else drops it).
    weak_self: std::sync::OnceLock<std::sync::Weak<LibeiSessionHandleImpl>>,
}

impl LibeiSessionHandleImpl {
    /// Activate EIS: call ConnectToEIS, handshake, and set up devices.
    ///
    /// Called on-demand when the first RDP client connects (via activate_input).
    /// This prevents the EIS socket from dying due to compositor idle timeout.
    async fn activate_eis(&self) -> Result<()> {
        // Prevent concurrent activation
        if self
            .activating
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            // Another task is already activating — wait for it
            while self.activating.load(std::sync::atomic::Ordering::Acquire) {
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            }
            return Ok(());
        }

        struct ActivationGuard(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for ActivationGuard {
            fn drop(&mut self) {
                self.0.store(false, std::sync::atomic::Ordering::Release);
            }
        }
        let _guard = ActivationGuard(Arc::clone(&self.activating));

        if self
            .eis_activated
            .load(std::sync::atomic::Ordering::Acquire)
        {
            // Check if socket is still alive
            let ctx = self.context.read().await;
            if let Some(ref c) = *ctx {
                if c.flush().is_ok() {
                    return Ok(());
                }
                warn!("[libei] EIS socket dead on reactivation, creating new session");
            }
            drop(ctx);
            // Socket is dead — reset state for fresh activation
            self.devices.clear().await;
            self.seats.lock().await.clear();
            self.eis_activated
                .store(false, std::sync::atomic::Ordering::Release);
        }

        info!("[libei] Activating EIS — calling ConnectToEIS");

        let session = self.portal_session.read().await;
        let fd = self
            .remote_desktop
            .connect_to_eis(
                &session,
                ashpd::desktop::remote_desktop::ConnectToEISOptions::default(),
            )
            .await
            .context("ConnectToEIS failed")?;
        drop(session);

        let stream = UnixStream::from(fd);
        let context = ei::Context::new(stream).context("Failed to create EIS context")?;

        let mut events =
            EiEventStream::new(context.clone()).context("Failed to create EIS event stream")?;

        let handshake_resp = reis::tokio::ei_handshake(
            &mut events,
            "lamco-rdp-server",
            ei::handshake::ContextType::Sender,
        )
        .await
        .context("EIS handshake failed")?;

        info!("[libei] EIS handshake complete");

        if let Err(e) = context.flush() {
            warn!("[libei] Context flush after handshake failed: {}", e);
        }

        // Store context
        {
            let mut ctx = self.context.write().await;
            *ctx = Some(context);
        }
        *self.connection.lock().await = Some(handshake_resp.connection);
        *self.devices.last_serial.lock().await = handshake_resp.serial;

        // Process initial events (seat + device setup) with a timeout.
        // The compositor sends all setup events in a burst. We process
        // until the stream goes quiet (no event within 500ms) or EOF.
        loop {
            match tokio::time::timeout(tokio::time::Duration::from_millis(500), events.next()).await
            {
                Ok(Some(Ok(PendingRequestResult::Request(event)))) => {
                    self.handle_event(event).await?;
                }
                Ok(Some(Ok(PendingRequestResult::ParseError(msg)))) => {
                    warn!("[libei] EIS parse error during setup: {}", msg);
                }
                Ok(Some(Ok(PendingRequestResult::InvalidObject(id)))) => {
                    debug!("[libei] Invalid object during setup: {}", id);
                }
                Ok(Some(Err(e))) => {
                    error!("[libei] EIS event stream error during setup: {}", e);
                    return Err(e.into());
                }
                Ok(None) => {
                    // Stream EOF — setup complete
                    debug!("[libei] Event stream EOF during setup");
                    break;
                }
                Err(_) => {
                    // Timeout — no more events, setup is done
                    debug!("[libei] No more events after 500ms, setup complete");
                    break;
                }
            }
        }

        info!("[libei] EIS setup complete — devices ready for input injection");
        self.eis_activated
            .store(true, std::sync::atomic::Ordering::Release);

        // Keep consuming events AFTER setup.
        // KWin re-creates the EIS absolute device on every output change
        // (outputsChanged → changeDevice: remove old + add new with fresh
        // regions). Dropping the EiEventStream here would make the
        // removal invisible and pointer_absolute would keep a DEAD device
        // handle forever — after a mid-session resolution switch, absolute
        // pointer injection goes into the void ("mouse defective"). The
        // persistent loop re-binds roles when the replacement device's Done
        // event arrives and answers compositor Pings.
        let weak_handle = self
            .weak_self
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("weak_self not initialized"))?;
        tokio::spawn(async move {
            let Some(handle) = weak_handle.upgrade() else {
                debug!("[libei] Handle dropped before EIS event task started");
                return;
            };
            loop {
                match events.next().await {
                    Some(Ok(PendingRequestResult::Request(event))) => {
                        if let Err(e) = handle.handle_event(event).await {
                            warn!("[libei] Event handling error: {}", e);
                        }
                    }
                    Some(Ok(PendingRequestResult::ParseError(msg))) => {
                        warn!("[libei] EIS parse error: {}", msg);
                    }
                    Some(Ok(PendingRequestResult::InvalidObject(id))) => {
                        debug!("[libei] Invalid object: {}", id);
                    }
                    Some(Err(e)) => {
                        // EOF or socket death — stop consuming; the next
                        // activate_input() call detects the dead socket via
                        // the flush probe and performs full recovery.
                        warn!("[libei] EIS event stream ended: {}", e);
                        handle
                            .eis_activated
                            .store(false, std::sync::atomic::Ordering::Release);
                        break;
                    }
                    None => {
                        debug!("[libei] EIS event stream EOF");
                        handle
                            .eis_activated
                            .store(false, std::sync::atomic::Ordering::Release);
                        break;
                    }
                }
            }
        });

        if let Some(r) = self.health_reporter.get() {
            r.report(HealthEvent::EisStreamRecovered);
        }

        Ok(())
    }

    async fn handle_event(&self, event: ei::Event) -> Result<()> {
        match event {
            ei::Event::Connection(_connection, request) => match request {
                ei::connection::Event::Seat { seat } => {
                    debug!("[libei] Seat added");
                    let mut seats = self.seats.lock().await;
                    seats.insert(seat, SeatData::default());
                }
                ei::connection::Event::Ping { ping } => {
                    ping.done(0);
                    if let Some(ref ctx) = *self.context.read().await {
                        let _ = ctx.flush();
                    }
                }
                _ => {}
            },

            ei::Event::Seat(seat, request) => {
                let mut seats = self.seats.lock().await;
                let Some(data) = seats.get_mut(&seat) else {
                    warn!("[libei] Received event for unknown seat, ignoring");
                    return Ok(());
                };

                match request {
                    ei::seat::Event::Name { name } => {
                        data.name = Some(name.clone());
                        debug!("[libei] Seat name: {}", name);
                    }
                    ei::seat::Event::Capability { mask, interface } => {
                        data.capabilities.insert(interface.clone(), mask);
                        debug!("[libei] Seat capability: {} (mask: {})", interface, mask);
                    }
                    ei::seat::Event::Done => {
                        let caps = data.capabilities.values().fold(0, |a, b| a | b);
                        seat.bind(caps);
                        if let Some(ref conn) = *self.connection.lock().await {
                            conn.sync(1);
                        }
                        if let Some(ref ctx) = *self.context.read().await {
                            let _ = ctx.flush();
                        }

                        info!(
                            "libei: Seat '{}' ready with capabilities: {:?}",
                            data.name.as_deref().unwrap_or("unknown"),
                            data.capabilities.keys().collect::<Vec<_>>()
                        );
                    }
                    ei::seat::Event::Device { device } => {
                        debug!("[libei] Device added to seat");
                        let mut devs = self.devices.all.lock().await;
                        devs.insert(
                            device,
                            DeviceData {
                                seat: Some(seat.clone()),
                                ..Default::default()
                            },
                        );
                    }
                    _ => {}
                }
            }

            ei::Event::Device(device, request) => {
                let mut devs = self.devices.all.lock().await;
                let Some(data) = devs.get_mut(&device) else {
                    warn!("[libei] Received event for unknown device, ignoring");
                    return Ok(());
                };

                match request {
                    ei::device::Event::Name { name } => {
                        data.name = Some(name.clone());
                        debug!("[libei] Device name: {}", name);
                    }
                    ei::device::Event::DeviceType { device_type } => {
                        data.device_type = Some(device_type);
                        debug!("[libei] Device type: {:?}", device_type);
                    }
                    ei::device::Event::Interface { object } => {
                        let interface_name = object.interface().to_owned();
                        data.interfaces.insert(interface_name.clone(), object);
                        info!("[libei] Device interface: {}", interface_name);
                    }
                    ei::device::Event::Region {
                        offset_x,
                        offset_y,
                        width,
                        hight,
                        scale,
                    } => {
                        info!(
                            "[libei] Device region: {}x{} at ({},{}) scale={}",
                            width, hight, offset_x, offset_y, scale
                        );
                        // REPLACE regions instead of appending —
                        // KWin re-creates the device on output changes, and a
                        // re-added device may carry updated regions for the
                        // SAME device object. Appending leaves stale (possibly
                        // larger) regions that offset absolute pointer
                        // coordinates into the void.
                        data.regions = vec![eis_common::DeviceRegion {
                            x: offset_x,
                            y: offset_y,
                            width,
                            height: hight,
                        }];
                    }
                    ei::device::Event::Destroyed { serial } => {
                        // KWin's
                        // outputsChanged → changeDevice removes the old
                        // device before adding a fresh one with new regions.
                        // Clear every role that held this device so injection
                        // re-binds when the replacement's Done event arrives;
                        // until then with_device_interface fails fast
                        // ("EIS pointer_absolute not ready") instead of
                        // injecting into a dead handle.
                        *self.devices.last_serial.lock().await = serial;
                        let removed = devs.remove(&device);
                        drop(devs);
                        if let Some(data) = removed {
                            let was_abs = self
                                .devices
                                .pointer_absolute
                                .lock()
                                .await
                                .as_ref()
                                .is_some_and(|d| *d == device);
                            if was_abs {
                                *self.devices.pointer_absolute.lock().await = None;
                                info!(
                                    "[libei] Absolute pointer device removed (output change?) — awaiting replacement"
                                );
                            }
                            let was_ptr = self
                                .devices
                                .pointer
                                .lock()
                                .await
                                .as_ref()
                                .is_some_and(|d| *d == device);
                            if was_ptr {
                                *self.devices.pointer.lock().await = None;
                                info!(
                                    "[libei] Pointer device removed (output change?) — awaiting replacement"
                                );
                            }
                            let was_kbd = self
                                .devices
                                .keyboard
                                .lock()
                                .await
                                .as_ref()
                                .is_some_and(|d| *d == device);
                            if was_kbd {
                                *self.devices.keyboard.lock().await = None;
                                info!(
                                    "[libei] Keyboard device removed (output change?) — awaiting replacement"
                                );
                            }
                            let was_touch = self
                                .devices
                                .touch
                                .lock()
                                .await
                                .as_ref()
                                .is_some_and(|d| *d == device);
                            if was_touch {
                                *self.devices.touch.lock().await = None;
                                info!(
                                    "[libei] Touch device removed (output change?) — awaiting replacement"
                                );
                            }
                            let _ = data;
                        }
                    }
                    ei::device::Event::Done => {
                        // Assign device roles (keyboard, pointer, touch).
                        // start_emulating is called in Resumed, not here --
                        // calling it in Done + Resumed causes KWin to reject
                        // the second call with "Invalid device state 3"
                        // and disconnect the client.
                        eis_common::assign_device_roles(&device, data, &self.devices).await;
                        info!(
                            "[libei] Device '{}' setup complete",
                            data.name.as_deref().unwrap_or("unknown"),
                        );
                    }
                    ei::device::Event::Resumed { serial } => {
                        *self.devices.last_serial.lock().await = serial;
                        device.start_emulating(serial.wrapping_add(1), serial);
                        if let Some(ref ctx) = *self.context.read().await {
                            let _ = ctx.flush();
                        }
                        info!("[libei] Device resumed and emulating (serial={})", serial);
                    }
                    _ => {}
                }
            }

            _ => {}
        }

        Ok(())
    }
}

#[async_trait]
impl SessionHandle for LibeiSessionHandleImpl {
    fn set_health_reporter(&self, reporter: HealthReporter) {
        let _ = self.health_reporter.set(reporter);
    }

    fn pipewire_access(&self) -> PipeWireAccess {
        // libei provides input only; video comes from Portal ScreenCast
        warn!(
            "libei: pipewire_access() called but this strategy provides input only. \
             Video capture requires Portal ScreenCast."
        );
        PipeWireAccess::NodeId(0)
    }

    fn streams(&self) -> Vec<StreamInfo> {
        futures::executor::block_on(async { self.streams.lock().await.clone() })
    }

    fn session_type(&self) -> SessionType {
        SessionType::Libei
    }

    async fn activate_input(&self) -> Result<()> {
        self.activate_eis().await
    }

    async fn notify_keyboard_keycode(&self, keycode: i32, pressed: bool) -> Result<()> {
        eis_common::eis_keyboard_keycode(&self.context, &self.devices, keycode, pressed).await
    }

    async fn notify_pointer_motion_absolute(&self, _stream_id: u32, x: f64, y: f64) -> Result<()> {
        eis_common::eis_pointer_motion_absolute(&self.context, &self.devices, x, y).await
    }

    async fn notify_pointer_button(&self, button: i32, pressed: bool) -> Result<()> {
        eis_common::eis_pointer_button(&self.context, &self.devices, button, pressed).await
    }

    async fn notify_pointer_axis(&self, dx: f64, dy: f64) -> Result<()> {
        eis_common::eis_pointer_axis(&self.context, &self.devices, dx, dy).await
    }

    async fn notify_pointer_motion_relative(&self, dx: f64, dy: f64) -> Result<()> {
        eis_common::eis_pointer_motion_relative(&self.context, &self.devices, dx, dy).await
    }

    async fn notify_touch_down(&self, _stream_id: u32, slot: u32, x: f64, y: f64) -> Result<()> {
        eis_common::eis_touch_down(&self.context, &self.devices, slot, x, y).await
    }

    async fn notify_touch_motion(&self, _stream_id: u32, slot: u32, x: f64, y: f64) -> Result<()> {
        eis_common::eis_touch_motion(&self.context, &self.devices, slot, x, y).await
    }

    async fn notify_touch_up(&self, slot: u32) -> Result<()> {
        eis_common::eis_touch_up(&self.context, &self.devices, slot).await
    }

    fn clipboard_source(&self) -> crate::session::strategy::ClipboardSource {
        crate::session::strategy::ClipboardSource::None
    }

    #[cfg(feature = "wl-clipboard")]
    async fn build_clipboard(
        &self,
        _portal_fallback: Option<crate::session::strategy::ClipboardComponents>,
        _rate_limit_ms: u64,
    ) -> Option<std::sync::Arc<dyn crate::clipboard::provider::ClipboardProvider>> {
        // Wayland data-control clipboard (wl-clipboard-rs) — no second Portal
        // session, so libei keeps EIS input without the duplicate-session
        // permission prompt / input-routing coupling.
        Some(std::sync::Arc::new(
            crate::clipboard::providers::WlClipboardProvider::new(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "Requires Portal RemoteDesktop"]
    async fn test_libei_availability() {
        let available = LibeiStrategy::is_available().await;
        println!("libei available: {available}");
    }

    #[tokio::test]
    #[ignore = "Requires active Portal session and user approval"]
    async fn test_create_session() {
        if !LibeiStrategy::is_available().await {
            println!("Skipping: libei not available");
            return;
        }

        let strategy = LibeiStrategy::new(None, None);
        match strategy.create_session().await {
            Ok(session) => {
                assert_eq!(session.session_type(), SessionType::Libei);
                println!("libei session created successfully");

                // Give event loop time to discover devices
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
            Err(e) => {
                println!("libei session creation failed: {e}");
            }
        }
    }
}
