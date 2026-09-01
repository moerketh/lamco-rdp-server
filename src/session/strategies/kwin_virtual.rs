//! KWin zkde-screencast virtual output strategy (KDE Plasma 6+).
//!
//! Native-resolution video for KDE: instead of capturing the physical output
//! (capped by hyperv_drm's fixed mode list) and scaling, this strategy asks
//! KWin to CREATE a virtual output at exactly the client's requested
//! resolution and stream it — via the private `zkde_screencast_unstable_v1`
//! protocol that xdg-desktop-portal-kde itself wraps.
//!
//! One request (`stream_virtual_output`) creates the output AND its PipeWire
//! stream — no portal session, no consent dialog, no source picker. Capture
//! size == desktop size, so the whole scaling/mode-switching machinery
//! (frame_scaler, kscreen-doctor parsing, inverse pointer mapping, stride
//! compaction) is bypassed by construction.
//!
//! Input reuses the libei machinery (EIS via Portal RemoteDesktop) — KWin's
//! only supported injection route. The input consent dialog still applies
//! (one-time via restore token); the VIDEO path is dialog-free.
//!
//! E2E validated 2026-09-01 (TEST_20260901180150): krfb-virtualmonitor +
//! portal picker + DRM-output-off produced a fully working native 1920x1200
//! session. This strategy is that recipe, done in-process and per-connection.
//!
//! Architecture:
//!
//! ```text
//! mstsc (client WxH)
//!    │
//!    ├─ video: zkde_screencast.stream_virtual_output("lamco", W, H, 1.0)
//!    │         → KWin creates Virtual-lamco @ WxH → stream `created(node)`
//!    │         → bind PipeWire node (shared daemon FD, MemFd buffers)
//!    │
//!    └─ input:  Portal RemoteDesktop + EIS (libei machinery, unchanged)
//! ```
//!
//! Wayland plumbing lives on a dedicated thread (the connection is not
//! async); commands flow in via std mpsc, results back via tokio oneshot.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::session::strategy::{
    ClipboardSource, PipeWireAccess, SessionHandle, SessionLifecyclePolicy, SessionType, StreamInfo,
};

/// Default output name KWin will assign/connect (`Virtual-<name>` in kscreen).
pub const OUTPUT_NAME: &str = "lamco";

/// Commands sent to the Wayland connection thread.
enum WlCommand {
    /// Create a virtual output at the given size; replies with the PipeWire node id.
    CreateStream {
        width: i32,
        height: i32,
        reply: tokio::sync::oneshot::Sender<Result<u32, String>>,
    },
    /// Close the current stream (destroys the virtual output server-side).
    Close,
}

/// Events reported from the Wayland thread back to the strategy.
struct WlState {
    /// Sender side for commands; None once the thread has exited.
    tx: Option<std::sync::mpsc::Sender<WlCommand>>,
}

/// The session handle: video state + libei input state.
pub struct KwinVirtualSessionHandle {
    /// Command channel to the Wayland thread.
    wl: RwLock<WlState>,
    /// The libei handle providing input injection (EIS).
    libei: Arc<crate::session::strategies::libei::LibeiSessionHandleImpl>,
    /// Current stream info (node id + geometry), updated on establish/release.
    streams: RwLock<Vec<StreamInfo>>,
    /// Set when the Wayland thread has died (compositor gone); next
    /// establish_for_client will rebuild it.
    wl_dead: AtomicBool,
}

impl KwinVirtualSessionHandle {
    fn new(libei: Arc<crate::session::strategies::libei::LibeiSessionHandleImpl>) -> Self {
        Self {
            wl: RwLock::new(WlState { tx: None }),
            libei,
            streams: RwLock::new(Vec::new()),
            wl_dead: AtomicBool::new(true),
        }
    }

    /// Ensure the Wayland thread exists, creating it if needed.
    async fn ensure_wl_thread(&self) -> Result<std::sync::mpsc::Sender<WlCommand>> {
        if self.wl_dead.load(Ordering::Acquire) {
            let mut guard = self.wl.write().await;
            if guard.tx.is_none() {
                let (tx, rx) = std::sync::mpsc::channel::<WlCommand>();
                std::thread::Builder::new()
                    .name("kwin-zkde-screencast".into())
                    .spawn(move || wayland_thread(rx))
                    .context("Failed to spawn zkde-screencast thread")?;
                guard.tx = Some(tx);
                self.wl_dead.store(false, Ordering::Release);
                info!("[kwin-virtual] Wayland thread started");
            }
        }
        self.wl
            .read()
            .await
            .tx
            .clone()
            .ok_or_else(|| anyhow::anyhow!("zkde-screencast thread unavailable"))
    }

    /// (Re-)create the virtual output stream at the given size.
    async fn recreate_stream(&self, width: u16, height: u16) -> Result<u32> {
        let tx = self.ensure_wl_thread().await?;

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(WlCommand::CreateStream {
            width: width as i32,
            height: height as i32,
            reply: reply_tx,
        })
        .map_err(|_| anyhow::anyhow!("zkde-screencast thread exited"))?;

        let node_id = tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx)
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for zkde stream creation"))?
            .map_err(|_| anyhow::anyhow!("zkde stream reply dropped"))?
            .map_err(|e| anyhow::anyhow!("zkde stream creation failed: {e}"))?;

        let info = StreamInfo {
            node_id,
            width: width as u32,
            height: height as u32,
            // While the virtual output is the only enabled output it sits at
            // (0,0); the strategy's output management ensures that.
            position_x: 0,
            position_y: 0,
        };
        *self.streams.write().await = vec![info];
        info!(
            "[kwin-virtual] virtual output '{}' @ {width}x{height} streaming on node {node_id}",
            OUTPUT_NAME
        );
        Ok(node_id)
    }
}

#[async_trait]
impl SessionHandle for KwinVirtualSessionHandle {
    fn pipewire_access(&self) -> PipeWireAccess {
        // The PipeWire node id is known only after establish_for_client; the
        // server calls this once at startup, where we have nothing yet.
        // Return the NodeId form when we have a stream, else a daemon FD the
        // same way (the node binding happens later in the pipeline).
        match self.streams.try_read() {
            Ok(s) if !s.is_empty() => PipeWireAccess::NodeId(s[0].node_id),
            _ => {
                // No stream yet — hand out a daemon connection; the pipeline
                // binds by node id once the stream is created.
                match crate::mutter::connect_to_pipewire_daemon() {
                    Ok(fd) => PipeWireAccess::FileDescriptor(fd),
                    // The pipeline will surface the error; not fatal here.
                    Err(_) => PipeWireAccess::NodeId(0),
                }
            }
        }
    }

    fn streams(&self) -> Vec<StreamInfo> {
        self.streams.blocking_read().clone()
    }

    fn session_type(&self) -> SessionType {
        SessionType::KwinVirtual
    }

    async fn notify_keyboard_keycode(&self, keycode: i32, pressed: bool) -> Result<()> {
        self.libei.notify_keyboard_keycode(keycode, pressed).await
    }

    async fn notify_pointer_motion_absolute(&self, stream_id: u32, x: f64, y: f64) -> Result<()> {
        self.libei
            .notify_pointer_motion_absolute(stream_id, x, y)
            .await
    }

    async fn notify_pointer_button(&self, button: i32, pressed: bool) -> Result<()> {
        self.libei.notify_pointer_button(button, pressed).await
    }

    async fn notify_pointer_axis(&self, dx: f64, dy: f64) -> Result<()> {
        self.libei.notify_pointer_axis(dx, dy).await
    }

    async fn notify_pointer_motion_relative(&self, dx: f64, dy: f64) -> Result<()> {
        self.libei.notify_pointer_motion_relative(dx, dy).await
    }

    async fn notify_touch_down(&self, stream_id: u32, slot: u32, x: f64, y: f64) -> Result<()> {
        self.libei.notify_touch_down(stream_id, slot, x, y).await
    }

    async fn notify_touch_motion(&self, stream_id: u32, slot: u32, x: f64, y: f64) -> Result<()> {
        self.libei.notify_touch_motion(stream_id, slot, x, y).await
    }

    async fn notify_touch_up(&self, slot: u32) -> Result<()> {
        self.libei.notify_touch_up(slot).await
    }

    async fn activate_input(&self) -> Result<()> {
        self.libei.activate_input().await
    }

    fn lifecycle_policy(&self) -> SessionLifecyclePolicy {
        // The virtual output exists only while an RDP client is connected;
        // KWin removes it when the stream closes (ScreencastManager removes
        // the output on stream finished). Re-establish per connection.
        SessionLifecyclePolicy::PerConnection
    }

    async fn establish_for_client(&self) -> Result<(Vec<StreamInfo>, bool)> {
        // Size: the previous connection's stream if present (reconnect at the
        // same size), else a sensible default. `request_initial_size` follows
        // immediately after with the client's actual request and resizes via
        // resize_capture_source, so this initial size only needs to be valid.
        let (w, h) = {
            let s = self.streams.read().await;
            s.first()
                .map_or((1920, 1200), |st| (st.width as u16, st.height as u16))
        };

        let _node = self.recreate_stream(w, h).await?;
        let streams = self.streams.read().await.clone();
        Ok((streams, true))
    }

    async fn release_after_client(&self) {
        // Close the stream — KWin destroys the virtual output on stream close.
        if let Some(tx) = self.wl.read().await.tx.as_ref() {
            let _ = tx.send(WlCommand::Close);
        }
        self.streams.write().await.clear();
        info!("[kwin-virtual] stream closed — virtual output removed");
    }

    async fn resize_capture_source(&self, width: u16, height: u16) -> Option<(u16, u16)> {
        // The virtual output is elastic: recreate it at the requested size
        // and the stream follows. zkde-screencast accepts ANY resolution —
        // this is the whole point of the strategy (no DRM mode list).
        if let Err(e) = self.recreate_stream(width, height).await {
            warn!("[kwin-virtual] resize to {width}x{height} failed: {e} — keeping current stream");
            let cur = self.streams.read().await;
            return cur.first().map(|s| (s.width as u16, s.height as u16));
        }
        Some((width, height))
    }

    fn clipboard_source(&self) -> ClipboardSource {
        ClipboardSource::None
    }
}

/// Wayland connection thread: owns the zkde-screencast objects.
///
/// Runs a blocking dispatch loop around a std mpsc of commands. The
/// `created`/`failed`/`closed` events of the stream object are collected
/// into per-request oneshot replies.
fn wayland_thread(rx: std::sync::mpsc::Receiver<WlCommand>) {
    use wayland_client::{Connection, Dispatch, QueueHandle, protocol::wl_registry};

    use wayland_protocols_plasma::screencast::v1::client::{
        zkde_screencast_stream_unstable_v1::Event as StreamEvent,
        zkde_screencast_stream_unstable_v1::ZkdeScreencastStreamUnstableV1,
        zkde_screencast_unstable_v1::{Event as ManagerEvent, ZkdeScreencastUnstableV1},
    };

    /// Per-thread dispatch state.
    struct State {
        screencast: Option<ZkdeScreencastUnstableV1>,
        /// Pending stream: the object + where to send the result.
        pending: Option<(
            ZkdeScreencastStreamUnstableV1,
            Option<tokio::sync::oneshot::Sender<Result<u32, String>>>,
        )>,
        /// Result already delivered (node id) — guards double-reply.
        done: bool,
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for State {
        fn event(
            state: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
            {
                if interface == "zkde_screencast_unstable_v1" {
                    // KWin advertises version 6; the plasma bindings (XML v4)
                    // cap us at 4 — bind min(server, 4).
                    let bind_version = version.min(4);
                    let screencast = registry.bind::<ZkdeScreencastUnstableV1, _, State>(
                        name,
                        bind_version,
                        qh,
                        (),
                    );
                    state.screencast = Some(screencast);
                    info!(
                        "[kwin-virtual] bound zkde_screencast_unstable_v1 (global v{version}, bound v{bind_version})"
                    );
                }
            }
        }
    }

    impl Dispatch<ZkdeScreencastUnstableV1, ()> for State {
        fn event(
            _: &mut Self,
            _: &ZkdeScreencastUnstableV1,
            _: ManagerEvent,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            // The manager object has no events.
        }
    }

    // NOTE: the manager interface has no events; a blanket empty impl is
    // impossible with the generic Dispatch trait, so we provide the one
    // above. The stream object's events are dispatched below via a macro-free
    // impl — see `Dispatch<ZkdeScreencastStreamUnstableV1, ()>`.

    impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for State {
        fn event(
            state: &mut Self,
            _stream: &ZkdeScreencastStreamUnstableV1,
            event: StreamEvent,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                StreamEvent::Created { node } => {
                    if !state.done {
                        state.done = true;
                        if let Some((_, Some(reply))) = state.pending.take() {
                            let _ = reply.send(Ok(node));
                        }
                        info!("[kwin-virtual] stream created: PipeWire node {node}");
                    }
                }
                StreamEvent::Failed { error } => {
                    if !state.done {
                        state.done = true;
                        if let Some((_, Some(reply))) = state.pending.take() {
                            let _ = reply.send(Err(error));
                        }
                        warn!("[kwin-virtual] stream failed");
                    }
                }
                StreamEvent::Closed => {
                    // Server-side close (compositor stopped the stream).
                    if !state.done {
                        state.done = true;
                        if let Some((_, Some(reply))) = state.pending.take() {
                            let _ = reply.send(Err("stream closed by compositor".into()));
                        }
                    }
                    info!("[kwin-virtual] stream closed by compositor");
                }
                _ => {}
            }
        }
    }

    impl Dispatch<wayland_client::protocol::wl_display::WlDisplay, ()> for State {
        fn event(
            _: &mut Self,
            _: &wayland_client::protocol::wl_display::WlDisplay,
            _: wayland_client::protocol::wl_display::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            error!("[kwin-virtual] cannot connect to Wayland: {e}");
            // Reply with errors until the channel drains, then exit.
            while let Ok(cmd) = rx.recv() {
                if let WlCommand::CreateStream { reply, .. } = cmd {
                    let _ = reply.send(Err(format!("wayland connection failed: {e}")));
                }
            }
            return;
        }
    };

    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    let display = conn.display();
    let _registry = display.get_registry(&qh, ());

    let mut state = State {
        screencast: None,
        pending: None,
        done: false,
    };

    // Initial roundtrip: binds the zkde global (if advertised).
    if let Err(e) = event_queue.roundtrip(&mut state) {
        error!("[kwin-virtual] initial roundtrip failed: {e}");
    }

    if state.screencast.is_none() {
        warn!(
            "[kwin-virtual] zkde_screencast_unstable_v1 not advertised — \
             is this KWin? (The global appears once the screencast plugin \
             has loaded; it may also be absent on non-KDE compositors.)"
        );
    }

    loop {
        // Drain commands first (non-blocking), then dispatch pending events.
        loop {
            match rx.try_recv() {
                Ok(WlCommand::CreateStream {
                    width,
                    height,
                    reply,
                }) => {
                    let Some(screencast) = state.screencast.as_ref() else {
                        let _ = reply.send(Err("zkde_screencast global not bound".into()));
                        continue;
                    };
                    // One stream at a time: close any previous.
                    if let Some((prev, _)) = state.pending.take() {
                        prev.close();
                    }
                    state.done = false;
                    let stream = screencast.stream_virtual_output(
                        OUTPUT_NAME.to_string(),
                        width,
                        height,
                        // scale: 1.0 — RDP clients express size in physical
                        // pixels; no compositor-side scaling wanted.
                        1.0,
                        // pointer mode: Hidden — RDP clients render their own
                        // cursor via pointer PDUs (matches the portal path).
                        1,
                        &qh,
                        (),
                    );
                    state.pending = Some((stream, Some(reply)));
                    if let Err(e) = conn.flush() {
                        warn!("[kwin-virtual] flush failed: {e}");
                    }
                }
                Ok(WlCommand::Close) => {
                    if let Some((stream, reply)) = state.pending.take() {
                        // No reply expected for Close; drop the pending
                        // oneshot silently.
                        drop(reply);
                        stream.close();
                        state.done = true;
                        if let Err(e) = conn.flush() {
                            warn!("[kwin-virtual] flush failed: {e}");
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // Strategy dropped the channel — exit thread.
                    info!("[kwin-virtual] command channel closed, thread exiting");
                    return;
                }
            }
        }

        // Blocking dispatch: waits for compositor events. Commands queued in
        // the meantime unblock via the next wake; the `created` reply is what
        // actually gates callers, so latency is fine.
        match event_queue.blocking_dispatch(&mut state) {
            Ok(_) => {}
            Err(e) => {
                error!("[kwin-virtual] dispatch failed: {e} — thread exiting");
                // Fail any pending request.
                if let Some((_, Some(reply))) = state.pending.take() {
                    let _ = reply.send(Err(format!("dispatch failed: {e}")));
                }
                return;
            }
        }
    }
}

// ============================================================================
// Strategy: session creation (composes libei input + virtual output video)
// ============================================================================

/// Output-management for the session: disable the physical (DRM) outputs so
/// the virtual output becomes the primary at (0,0) — the layout that made the
/// E2E session fully interactive (panel+windows relocate; pointer coordinate
/// chain closes). Re-enables them on drop, physical FIRST (sunshine rule:
/// never leave the host blind).
///
/// `kscreen-doctor` is invoked in a blocking thread; both directions are
/// best-effort — if it fails, the session still works (the virtual output is
/// usable as a secondary screen, just with the panel elsewhere).
struct OutputLayoutGuard {
    /// Connector names that were disabled by this guard.
    disabled: Vec<String>,
}

impl OutputLayoutGuard {
    /// Snapshot enabled non-virtual outputs, then disable them.
    async fn engage() -> Self {
        let names = tokio::task::spawn_blocking(list_enabled_physical_outputs)
            .await
            .unwrap_or_default();
        if names.is_empty() {
            info!("[kwin-virtual] no physical outputs to manage (already headless?)");
            return Self {
                disabled: Vec::new(),
            };
        }
        info!(
            "[kwin-virtual] disabling physical output(s) for session: [{}]",
            names.join(", ")
        );
        for name in &names {
            let n = name.clone();
            let _ = tokio::task::spawn_blocking(move || disable_output(&n))
                .await
                .unwrap_or(false);
        }
        Self { disabled: names }
    }
}

impl Drop for OutputLayoutGuard {
    fn drop(&mut self) {
        // Physical FIRST — the machine must never be left without a display
        // if the virtual output died first.
        let names = std::mem::take(&mut self.disabled);
        for name in &names {
            let n = name.clone();
            let _ = std::thread::spawn(move || {
                enable_output(&n);
            })
            .join();
        }
        if !names.is_empty() {
            info!(
                "[kwin-virtual] physical output(s) re-enabled: [{}]",
                names.join(", ")
            );
        }
    }
}

/// Parse `kscreen-doctor -o` output: names of ENABLED outputs that are not
/// our virtual one. Best-effort — returns empty on any failure.
fn list_enabled_physical_outputs() -> Vec<String> {
    let out = match std::process::Command::new("kscreen-doctor")
        .arg("-o")
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => return Vec::new(),
    };

    // The text form is a sequence of blocks:
    //   Output: 1 Virtual-1\n    enabled\n    connected\n...
    // Walk blocks; collect "Output: N <name>" where the block contains
    // "enabled" and the name doesn't start with "Virtual-lamco" (ours).
    let mut result = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_enabled = false;
    for line in out.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Output: ") {
            // Flush previous block.
            if let (Some(name), true) = (&current_name, current_enabled) {
                if name != OUTPUT_NAME && !name.starts_with("Virtual-") {
                    result.push(name.clone());
                }
            }
            // "1 Virtual-1" -> name is everything after the index.
            current_name = rest.split_once(' ').map(|(_, n)| n.to_string());
            current_enabled = false;
        } else if line == "enabled" {
            current_enabled = true;
        }
    }
    if let (Some(name), true) = (&current_name, current_enabled) {
        if name != OUTPUT_NAME && !name.starts_with("Virtual-") {
            result.push(name.clone());
        }
    }
    result
}

fn disable_output(name: &str) -> bool {
    std::process::Command::new("kscreen-doctor")
        .arg(format!("output.{name}.disable"))
        .stdin(std::process::Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn enable_output(name: &str) -> bool {
    std::process::Command::new("kscreen-doctor")
        .arg(format!("output.{name}.enable"))
        .stdin(std::process::Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Session strategy: KWin zkde-screencast virtual output.
pub struct KwinVirtualStrategy {
    /// Token manager for the libei restore token (input consent).
    token_manager: Option<Arc<crate::session::token_manager::Tokens>>,
    /// Holds the output-layout guard once a session is live.
    layout: RwLock<Option<Arc<OutputLayoutGuard>>>,
}

impl KwinVirtualStrategy {
    pub fn new(token_manager: Option<Arc<crate::session::token_manager::Tokens>>) -> Self {
        Self {
            token_manager,
            layout: RwLock::new(None),
        }
    }

    /// Availability: KDE + Wayland + kscreen-doctor (for output management) +
    /// the zkde-screencast global being bindable.
    pub async fn is_available() -> bool {
        // Cheap environment check first — skip the Wayland probe elsewhere.
        let xdg = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        let is_kde = xdg.contains("KDE") || std::env::var("KDE_FULL_SESSION").is_ok();
        let is_wayland = std::env::var("WAYLAND_DISPLAY").is_ok();
        if !(is_kde && is_wayland) {
            return false;
        }
        // kscreen-doctor is needed for output management; absence only
        // degrades (panel elsewhere) — treat as available anyway. The
        // definitive probe is the zkde global, done on the connection thread
        // at establish time (the global only appears after the screencast
        // plugin loads, so a pre-flight probe is unreliable — see the
        // 2026-09-01 spike notes).
        true
    }
}

#[async_trait]
impl crate::session::strategy::SessionStrategy for KwinVirtualStrategy {
    fn name(&self) -> &'static str {
        "kwin-virtual"
    }

    fn requires_initial_setup(&self) -> bool {
        // The input (libei) consent is the one-time setup; video is dialog-free.
        true
    }

    fn supports_unattended_restore(&self) -> bool {
        // libei restore token covers the input consent.
        true
    }

    async fn create_session(&self) -> Result<Arc<dyn SessionHandle>> {
        info!("[kwin-virtual] creating session (zkde-screencast video + libei input)");

        // Input: reuse the libei machinery verbatim (Portal RemoteDesktop +
        // EIS, restore token, persistent event consumer — all the Bug-5
        // machinery). The concrete handle lets us delegate input directly.
        let libei_strategy =
            crate::session::strategies::libei::LibeiStrategy::new(None, self.token_manager.clone());
        let libei_impl = libei_strategy.create_session_concrete().await?;

        // Output management: engage the layout guard NOW (session start) so
        // the virtual output lands at (0,0) as the only enabled screen from
        // the first frame. Dropped on cleanup — physical outputs re-enable first.
        let guard = OutputLayoutGuard::engage().await;
        *self.layout.write().await = Some(Arc::new(guard));

        let handle = Arc::new(KwinVirtualSessionHandle::new(libei_impl));
        Ok(handle as Arc<dyn SessionHandle>)
    }

    async fn cleanup(&self, _session: &dyn SessionHandle) -> Result<()> {
        // Drop the layout guard — physical outputs re-enable first.
        self.layout.write().await.take();
        info!("[kwin-virtual] session cleanup complete (physical outputs restored)");
        Ok(())
    }
}

// NOTE: no adapter needed — create_session_concrete guarantees the concrete
// libei handle type for direct input delegation.
