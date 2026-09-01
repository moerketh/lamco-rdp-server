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
/// State machine for one zkde stream request: which conclusive event
/// (Created/Failed/Closed) has arrived, if any.
///
/// Invariants (exactly what the off-compositor tests pin):
/// 1. The FIRST conclusive event decides the outcome; later events are
///    ignored (a `Closed` following a `Failed` must not double-deliver).
/// 2. A new request (`reset`) re-arms the machine.
///
/// Hoisted to module scope so the logic is directly unit-testable — the
/// Wayland objects can't exist off-compositor, but these rules can and
/// must be tested (the double-delivery bug class is silent: a second
/// `reply.send` on a consumed oneshot is a no-op that masks real
/// state confusion).
#[derive(Debug)]
struct StreamRequestMachine {
    /// A conclusive event has already been delivered for this request.
    done: bool,
}

impl StreamRequestMachine {
    fn new() -> Self {
        Self { done: false }
    }

    /// Re-arm for a fresh request (new stream_virtual_output call).
    fn reset(&mut self) {
        self.done = false;
    }

    /// Apply a stream event: returns the outcome to deliver if this event
    /// is the FIRST conclusive one, else None. See the struct docs for the
    /// invariants.
    fn transition(
        &mut self,
        event: &wayland_protocols_plasma::screencast::v1::client::zkde_screencast_stream_unstable_v1::Event,
    ) -> Option<Result<u32, String>> {
        use wayland_protocols_plasma::screencast::v1::client::zkde_screencast_stream_unstable_v1::Event;
        if self.done {
            // Late event after conclusion: log Closed for observability only.
            if matches!(event, Event::Closed) {
                info!("[kwin-virtual] stream closed by compositor (request already concluded)");
            }
            return None;
        }
        match event {
            Event::Created { node } => {
                self.done = true;
                info!("[kwin-virtual] stream created: PipeWire node {node}");
                Some(Ok(*node))
            }
            Event::Failed { error } => {
                self.done = true;
                warn!("[kwin-virtual] stream failed: {error}");
                Some(Err(error.clone()))
            }
            Event::Closed => {
                self.done = true;
                info!("[kwin-virtual] stream closed by compositor");
                Some(Err("stream closed by compositor".into()))
            }
            _ => None,
        }
    }
}

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
        /// Stream request state machine (conclusive-event bookkeeping).
        stream_sm: StreamRequestMachine,
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
            // Pure transition, then deliver: the state machine decides if
            // the event concludes the pending request (first conclusive
            // event wins); delivery consumes the pending reply channel.
            if let Some(outcome) = state.stream_sm.transition(&event) {
                if let Some((_, Some(reply))) = state.pending.take() {
                    let _ = reply.send(outcome);
                }
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
        stream_sm: StreamRequestMachine::new(),
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
                    state.stream_sm.reset();
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
                        state.stream_sm.reset();
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
/// OUR virtual one. Best-effort — returns empty on any failure.
///
/// NOTE on naming: the exclusion is EXACT (`Virtual-lamco` — the name KWin
/// assigns our zkde-created output). It must NOT be a "Virtual-" prefix
/// match: hyperv_drm's connector is itself named `Virtual-1`, and that IS
/// a physical output this guard must manage (disabling it is the whole
/// point — panel relocation + origin placement). A prefix exclusion would
/// skip the DRM output entirely and leave the two-screen layout that broke
/// clicks in the E2E.
fn list_enabled_physical_outputs() -> Vec<String> {
    let out = match std::process::Command::new("kscreen-doctor")
        .arg("-o")
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => return Vec::new(),
    };

    parse_enabled_physical_outputs(&out)
}

/// Pure parser: given `kscreen-doctor -o` text, return enabled output names
/// excluding our own virtual output (`Virtual-{OUTPUT_NAME}`).
///
/// The text form is a sequence of blocks:
/// ```text
/// Output: 1 Virtual-1
///     enabled
///     connected
///     priority 1
///     ...
/// ```
/// We walk blocks; an "Output: N <name>" line opens a block, and a bare
/// "enabled" line marks it enabled. Only enabled, non-virtual names are
/// collected, in order.
fn parse_enabled_physical_outputs(kscreen_text: &str) -> Vec<String> {
    /// The exact name KWin gives our zkde-created virtual output.
    const VIRTUAL_OUTPUT_NAME: &str = "Virtual-lamco";

    let mut result = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_enabled = false;
    for line in kscreen_text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Output: ") {
            // Flush previous block.
            if let (Some(name), true) = (&current_name, current_enabled) {
                if name != VIRTUAL_OUTPUT_NAME {
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
    // Flush the final block (no trailing "Output:" line).
    if let (Some(name), true) = (&current_name, current_enabled) {
        if name != VIRTUAL_OUTPUT_NAME {
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

#[cfg(test)]
mod tests {
    //! kwin-virtual strategy unit tests.
    //!
    //! The Wayland object layer can't be exercised off-compositor, so the
    //! testable seams are the pure logic: the kscreen output parser (whose
    //! Virtual-1 exclusion bug shipped once — see the regression test) and
    //! the stream-event state machine (Created/Failed/Closed ordering
    //! rules).

    use super::*;

    // ========================================================================
    // parse_enabled_physical_outputs
    // ========================================================================

    /// Real-world sample from the Hyper-V VM (2026-09-01 E2E): hyperv_drm's
    /// connector is named `Virtual-1` and MUST be managed (disabled) by the
    /// guard; our zkde output is `Virtual-lamco` and MUST be excluded.
    /// The first shipped version excluded ANY `Virtual-*` name — skipping the
    /// DRM output entirely and leaving the two-screen layout that broke
    /// clicks. This sample is that exact machine's output.
    const KSCREEN_TWO_OUTPUTS: &str = "Output: 1 Virtual-1\n        enabled\n        connected\n        priority 1\n        Unknown\n        Modes:  1:1024x768@60!  2:1920x1080@60*  3:1600x1200@60 \n        Geometry: 0,0 1920x1080\n        Scale: 1\nOutput: 2 Virtual-lamco\n        enabled\n        connected\n        priority 2\n        Unknown\n        Modes:  25:1920x1200@60*! \n        Geometry: 1920,0 1920x1200\n        Scale: 1\n";

    #[test]
    fn test_parser_hyperv_drm_output_is_managed() {
        // The critical regression: hyperv_drm's `Virtual-1` is a PHYSICAL
        // output here — it must appear (be disabled by the guard), not be
        // excluded by a Virtual- prefix.
        let names = parse_enabled_physical_outputs(KSCREEN_TWO_OUTPUTS);
        assert!(
            names.contains(&"Virtual-1".to_string()),
            "hyperv_drm's Virtual-1 must be managed; got {names:?}"
        );
    }

    #[test]
    fn test_parser_excludes_own_virtual_output() {
        let names = parse_enabled_physical_outputs(KSCREEN_TWO_OUTPUTS);
        assert!(
            !names.contains(&"Virtual-lamco".to_string()),
            "our own virtual output must NOT be disabled; got {names:?}"
        );
    }

    #[test]
    fn test_parser_single_drm_output() {
        let text = "Output: 1 Virtual-1\n        enabled\n        connected\n        Geometry: 0,0 1920x1080\n";
        let names = parse_enabled_physical_outputs(text);
        assert_eq!(names, vec!["Virtual-1".to_string()]);
    }

    #[test]
    fn test_parser_skips_disabled_outputs() {
        // A disabled output (e.g. the guard's own prior work, or a DPMS-off
        // monitor) must not be collected — enabling it later is harmless but
        // re-disable bookkeeping relies on the list being exactly "currently
        // enabled".
        let text = "Output: 1 Virtual-1\n        enabled\nOutput: 2 HDMI-A-1\n        disabled\n";
        let names = parse_enabled_physical_outputs(text);
        assert_eq!(names, vec!["Virtual-1".to_string()]);
    }

    #[test]
    fn test_parser_real_connector_names() {
        // Bare-metal KDE naming (DP-1/HDMI-A-1) — the common case.
        let text = "Output: 1 DP-1\n        enabled\nOutput: 2 HDMI-A-1\n        enabled\nOutput: 3 Virtual-lamco\n        enabled\n";
        let names = parse_enabled_physical_outputs(text);
        assert_eq!(names, vec!["DP-1".to_string(), "HDMI-A-1".to_string()]);
    }

    #[test]
    fn test_parser_empty_and_garbage_input() {
        assert!(parse_enabled_physical_outputs("").is_empty());
        assert!(parse_enabled_physical_outputs("random noise\nno outputs here\n").is_empty());
        // "enabled" without a preceding Output block is ignored.
        assert!(parse_enabled_physical_outputs("enabled\nenabled\n").is_empty());
    }

    #[test]
    fn test_parser_no_trailing_newline() {
        // The final block flush must not require a trailing newline after
        // the last block — kscreen output shape varies.
        let text = "Output: 1 Virtual-1\n        enabled";
        let names = parse_enabled_physical_outputs(text);
        assert_eq!(names, vec!["Virtual-1".to_string()]);
    }

    // ========================================================================
    // Stream request state machine (StreamRequestMachine)
    // ========================================================================

    fn stream_event(
        kind: &str,
    ) -> wayland_protocols_plasma::screencast::v1::client::zkde_screencast_stream_unstable_v1::Event
    {
        stream_event_with_node(kind, 42)
    }

    fn stream_event_with_node(
        kind: &str,
        node: u32,
    ) -> wayland_protocols_plasma::screencast::v1::client::zkde_screencast_stream_unstable_v1::Event
    {
        use wayland_protocols_plasma::screencast::v1::client::zkde_screencast_stream_unstable_v1::Event;
        match kind {
            "created" => Event::Created { node },
            "failed" => Event::Failed {
                error: "compositor refused".to_string(),
            },
            "closed" => Event::Closed,
            _ => unreachable!("unknown kind {kind}"),
        }
    }

    #[test]
    fn test_stream_sm_created_succeeds_once() {
        let mut sm = StreamRequestMachine::new();
        // First Created delivers the node id.
        assert_eq!(sm.transition(&stream_event("created")), Some(Ok(42)));
        // A second event of any kind is ignored (no double-delivery).
        assert_eq!(sm.transition(&stream_event("closed")), None);
        assert_eq!(sm.transition(&stream_event("created")), None);
    }

    #[test]
    fn test_stream_sm_failed_fails_once_then_closed_ignored() {
        let mut sm = StreamRequestMachine::new();
        // Failed delivers the compositor's error message.
        assert_eq!(
            sm.transition(&stream_event("failed")),
            Some(Err("compositor refused".to_string()))
        );
        // A subsequent Closed (compositor closing the failed stream's
        // object) must NOT deliver again — the reply channel is consumed.
        assert_eq!(sm.transition(&stream_event("closed")), None);
    }

    #[test]
    fn test_stream_sm_closed_fails_with_generic_error() {
        let mut sm = StreamRequestMachine::new();
        // Closed before any other event: the request fails with the
        // generic message (no payload on the protocol event).
        assert_eq!(
            sm.transition(&stream_event("closed")),
            Some(Err("stream closed by compositor".to_string()))
        );
    }

    #[test]
    fn test_stream_sm_reset_rearms_after_conclusion() {
        // The per-connection lifecycle: Close then CreateStream re-arms the
        // machine; the new request must be able to deliver again.
        let mut sm = StreamRequestMachine::new();
        assert_eq!(
            sm.transition(&stream_event("failed")),
            Some(Err("compositor refused".to_string()))
        );
        sm.reset();
        assert_eq!(
            sm.transition(&stream_event_with_node("created", 7)),
            Some(Ok(7))
        );
        // And once more: concluded again.
        assert_eq!(sm.transition(&stream_event("closed")), None);
    }

    #[test]
    fn test_stream_sm_reset_on_fresh_request_allows_new_outcome() {
        // Mirrors the Close command path: reset without a conclusive event
        // (explicit user Close), then a fresh request succeeds.
        let mut sm = StreamRequestMachine::new();
        sm.reset(); // Close path resets even without an event.
        assert_eq!(
            sm.transition(&stream_event_with_node("created", 99)),
            Some(Ok(99))
        );
    }

    // ========================================================================
    // Strategy surface
    // ========================================================================

    #[test]
    fn test_output_name_constant() {
        // KWin prefixes "Virtual-" to the name we pass: stream_virtual_output
        // receives OUTPUT_NAME, and kscreen lists "Virtual-{OUTPUT_NAME}".
        // The parser's exclusion constant must match that construction.
        assert_eq!(OUTPUT_NAME, "lamco");
        assert_eq!(
            "Virtual-OUTPUT",
            format!("Virtual-{OUTPUT_NAME}").replace("lamco", "OUTPUT")
        );
        // The parser excludes exactly "Virtual-lamco".
        let text = "Output: 1 Virtual-lamco\n        enabled\n";
        assert!(parse_enabled_physical_outputs(text).is_empty());
    }

    #[tokio::test]
    async fn test_resize_capture_source_default_is_none() {
        // The trait's default (non-elastic strategies) must be None —
        // the display handler falls back to the DRM mode-switch path.
        // Use a minimal anonymous handle to prove the default.
        struct NoElastic;
        #[async_trait]
        impl crate::session::strategy::SessionHandle for NoElastic {
            fn pipewire_access(&self) -> crate::session::strategy::PipeWireAccess {
                crate::session::strategy::PipeWireAccess::NodeId(0)
            }
            fn streams(&self) -> Vec<StreamInfo> {
                Vec::new()
            }
            fn session_type(&self) -> SessionType {
                SessionType::Portal
            }
            async fn notify_keyboard_keycode(&self, _k: i32, _p: bool) -> Result<()> {
                Ok(())
            }
            async fn notify_pointer_motion_absolute(
                &self,
                _s: u32,
                _x: f64,
                _y: f64,
            ) -> Result<()> {
                Ok(())
            }
            async fn notify_pointer_button(&self, _b: i32, _p: bool) -> Result<()> {
                Ok(())
            }
            async fn notify_pointer_axis(&self, _dx: f64, _dy: f64) -> Result<()> {
                Ok(())
            }
            fn clipboard_source(&self) -> ClipboardSource {
                ClipboardSource::None
            }
        }
        let h = NoElastic;
        assert!(h.resize_capture_source(1920, 1200).await.is_none());
    }
}
