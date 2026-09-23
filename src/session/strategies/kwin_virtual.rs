//! KWin zkde-screencast virtual output strategy (KDE Plasma 6+).
//!
//! The zkde-screencast Wayland machinery — virtual-output creation via the
//! private `zkde_screencast_unstable_v1` protocol, the create-before-close
//! stream lifecycle, and the kscreen physical-output layout management —
//! lives in the `hyperv-rdp-extras` crate (MIT; see that repo's
//! PROVENANCE.md). This module keeps the strategy shell: the libei input
//! composition and the `SessionHandle` implementation.
//!
//! One request (`stream_virtual_output`) creates the output AND its
//! PipeWire stream — no portal session, no consent dialog, no source
//! picker. Capture size == desktop size, so no capture-to-desktop scaling,
//! coordinate remapping, or stride compaction machinery is needed between
//! the stream and the encoder.
//!
//! Input reuses the libei machinery (EIS via Portal RemoteDesktop) — KWin's
//! only supported injection route. The input consent dialog still applies
//! (one-time via restore token); the VIDEO path is dialog-free.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::RwLock;
use tracing::info;

use crate::session::strategy::{
    ClipboardSource, PipeWireAccess, SessionHandle, SessionLifecyclePolicy, SessionType, StreamInfo,
};
use hyperv_rdp_extras::session::{
    ModeChangeOutcome, OutputLayoutGuard, VirtualOutputConfig, VirtualOutputManager,
};

use crate::session::strategy::{CaptureResizeEffect, CaptureResizeOutcome};

/// In-place virtual-output mode change (experiment D) instead of
/// destroy/recreate on elastic resize. On by default; set
/// `LAMCO_KWIN_INPLACE_MODE=0` as an emergency off-switch if an untested
/// compositor mishandles the in-place request. Read once per resize.
fn in_place_mode_change_enabled() -> bool {
    !matches!(std::env::var("LAMCO_KWIN_INPLACE_MODE").as_deref(), Ok("0"))
}

// Re-exported for the parser tests below and external callers. The kscreen
// parser is parameterized by the excluded kscreen name (exact match; see
// the crate docs) — the fork's output identity stays "lamco".
pub use hyperv_rdp_extras::session::{parse_enabled_physical_outputs, strip_ansi};

/// The output name this fork passes to `stream_virtual_output`; KWin lists
/// the output as `Virtual-{OUTPUT_NAME}` in kscreen. Behaviorally pinned
/// by the parser tests below.
pub const OUTPUT_NAME: &str = "lamco";

/// The kscreen exclusion name matching [`OUTPUT_NAME`].
pub const VIRTUAL_OUTPUT_KSCREEN_NAME: &str = "Virtual-lamco";

/// The session handle: video state + libei input state.
pub struct KwinVirtualSessionHandle {
    // FIELD ORDER IS LOAD-BEARING: Rust drops fields in declaration order.
    // layout_guard MUST come first so an ABNORMAL drop (teardown without
    // release_after_client — mid-establish failure, error path) re-enables
    // the physical outputs BEFORE wl's VirtualOutputManager destroys the
    // zkde virtual output. The normal path (release_after_client) does the
    // same order explicitly; this makes the implicit path match.
    /// Output layout guard for THIS connection: engaged on
    /// establish_for_client (console stays visible while the server
    /// idles), dropped on release_after_client (physical outputs
    /// re-enable first — the sunshine rule).
    layout_guard: RwLock<Option<Arc<OutputLayoutGuard>>>,
    /// Virtual-output stream manager (Wayland thread + create-before-close
    /// lifecycle; crate-owned).
    wl: RwLock<VirtualOutputManager>,
    /// The libei handle providing input injection (EIS).
    libei: Arc<crate::session::strategies::libei::LibeiSessionHandleImpl>,
    /// Current stream info (node id + geometry), updated on establish/release.
    streams: RwLock<Vec<StreamInfo>>,
    /// Last size the virtual output was (re)created at — the connect-time
    /// pre-warm prediction for the next client (see establish_for_client).
    /// std Mutex: tiny payload, no await held across the guard.
    last_output_size: std::sync::Mutex<Option<(u16, u16)>>,
}

impl KwinVirtualSessionHandle {
    fn new(libei: Arc<crate::session::strategies::libei::LibeiSessionHandleImpl>) -> Self {
        Self {
            wl: RwLock::new(VirtualOutputManager::with_config(VirtualOutputConfig::new(
                OUTPUT_NAME,
            ))),
            libei,
            streams: RwLock::new(Vec::new()),
            layout_guard: RwLock::new(None),
            last_output_size: std::sync::Mutex::new(None),
        }
    }

    /// (Re-)create the virtual output stream at the given size.
    async fn recreate_stream(&self, width: u16, height: u16) -> Result<u32> {
        // The crate's manager owns the Wayland thread (created on demand),
        // the create-before-close swap, the enable-after-every-create
        // black-screen guard, and the created-reply timeout.
        let node_id = self.wl.read().await.recreate_stream(width, height).await?;

        // Remember the created size as the next connection's pre-warm
        // prediction (create-before-close means the recreate only returned
        // once the new output exists).
        *self
            .last_output_size
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some((width, height));

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
        // Sync trait method over async state. The runtime forbids
        // blocking_* on its own threads (tokio panics: "Cannot block the
        // current thread from within a runtime"). futures::executor::block_on
        // drives the lock's future on THIS thread without registering with
        // the runtime, which is safe here (the lock is only held briefly by
        // establish/release) and is the same pattern the libei handle uses
        // for its streams().
        futures::executor::block_on(async { self.streams.read().await.clone() })
    }

    fn session_type(&self) -> SessionType {
        SessionType::KwinVirtual
    }

    /// Layout heal for the kwin-virtual strategy: normalize the virtual
    /// output to the origin, reattach orphaned plasmashell containments
    /// (the Plasma 6.3 fix), and restart plasmashell only when
    /// escalated (repeat heals). Triggered by the display handler's
    /// blank/panel-less capture recovery — see the detector's
    /// commentary for the full chain.
    async fn heal_output_layout(&self, escalate_restart: bool) -> bool {
        hyperv_rdp_extras::session::heal_output_layout(
            VIRTUAL_OUTPUT_KSCREEN_NAME,
            escalate_restart,
        )
        .await
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
        // IDEMPOTENT: REUSE a live stream instead of recreating it. Every
        // accepted socket runs this -- including vmconnect's throwaway
        // pre-connect probes (connect, zero bytes, gone), which arrive in
        // pairs right before the real client. Each recreate destroys the
        // current virtual output before the new one exists, and with the
        // physical output already disabled by the layout guard that
        // destroy-to-created window leaves ZERO enabled outputs --
        // plasmashell reacts by switching to its placeholder screen and
        // (field-observed 2026-09-04) never re-latches onto the replacement
        // output, so the session streams untouched all-zero buffers
        // forever: black screen on a fully healthy pipeline. Probes must
        // not churn the desktop. The stream is recreated only after
        // release_after_client (per-connection lifecycle) or when
        // resize_capture_source changes the size.
        {
            let s = self.streams.read().await;
            if let Some(first) = s.first() {
                info!(
                    "[kwin-virtual] reusing live stream (node {}, {}x{})",
                    first.node_id, first.width, first.height
                );
                return Ok((s.clone(), false));
            }
        }

        // Fresh establish (no live stream): pre-warm at the size the next
        // client is PREDICTED to want — the previous connection's size —
        // instead of a fixed default. The prediction pays off on the
        // overwhelmingly common case (same client reconnecting at the same
        // resolution): the virtual output is created at the client's size
        // HERE at accept time, so plasmashell's multi-second latch
        // overlaps the RDP handshake and — the bigger win —
        // request_initial_size's resize_capture_source short-circuits on
        // the size match, SKIPPING the destroy+recreate that used to add a
        // second full KWin relayout after caps negotiation (measured: the
        // create-at-default + recreate-at-request pair double-relayouts
        // every fresh connect). A mispredicted size costs exactly what
        // connect cost before this change (one recreate at caps) — never
        // more; and recreate_stream below still records the truth for the
        // next round.
        let (w, h) = self
            .last_output_size
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .unwrap_or((1920u16, 1200u16));
        info!("[kwin-virtual] fresh establish — pre-warming virtual output at {}x{}", w, h);

        // CREATE the virtual output FIRST (the crate's recreate_stream
        // also ensures it is ENABLED), then disable the physical one. Order
        // matters twice over:
        //
        // 1. zkde's stream_virtual_output can create the output in a
        //    DISABLED state — notably when a previous manual
        //    `kscreen-doctor output.Virtual-lamco.disable` persisted to
        //    kwinoutputconfig.json, every later output is born disabled.
        //    A disabled virtual output does not count as "an enabled
        //    output", so KWin then refuses the physical disable with
        //    "Disabling all outputs through configuration changes is not
        //    allowed" — the desktop stays on Virtual-1 and the captured
        //    virtual area is empty: black screen.
        // 2. kscreen-doctor refuses to disable the ONLY enabled output
        //    (same guard, for the born-enabled case before the virtual is
        //    up).
        //
        // So: create (→ enabled inside the crate's recreate_stream;
        // position/mode are already right from creation) → disable
        // physical.
        let _node = self.recreate_stream(w, h).await?;

        if self.layout_guard.read().await.is_none() {
            // engage_with, not engage(): the guard's kscreen exclusion must
            // match THIS strategy's output identity (Virtual-lamco) — the
            // crate's neutral default (Virtual-rdp) would leave our own
            // output unexcluded, and the guard would disable it as if it
            // were a physical output.
            let guard = OutputLayoutGuard::engage_with(VirtualOutputConfig::new(OUTPUT_NAME)).await;
            *self.layout_guard.write().await = Some(Arc::new(guard));
        }

        let streams = self.streams.read().await.clone();
        Ok((streams, true))
    }

    async fn release_after_client(&self) {
        // RESTORE THE PHYSICAL OUTPUTS *BEFORE* CLOSING THE STREAM — and
        // SETTLE between the two.
        //
        // The stream close destroys the zkde virtual output. KWin applies
        // the physical re-enable instantly, but clients bind the
        // re-announced wl_output ASYNCHRONOUSLY — closing the virtual
        // output in the same breath can beat that bind: Qt sees zero
        // outputs, creates its placeholder screen, and (KWin 6.7-era
        // shells) never re-latches. Every reconnect after such a teardown
        // renders a dead shell (field-observed on resolution-change
        // disconnects). guard.finish() restores AND settles (750ms,
        // mirroring the engage settle) so the removal becomes a plain
        // output change for plasmashell.
        {
            let mut guard_slot = self.layout_guard.write().await;
            if let Some(guard) = guard_slot.take() {
                // The handle is the guard's only share (diagnostic readers
                // clone-and-drop); try_unwrap yields the owned guard so
                // finish() can restore+settle. On a stray clone, fall back
                // to Drop-only restore (no settle — logged, not silent).
                match std::sync::Arc::try_unwrap(guard) {
                    Ok(mut owned) => owned.finish().await,
                    Err(shared) => {
                        tracing::warn!(
                            "[kwin-virtual] guard still shared at release — restoring without settle"
                        );
                        drop(shared);
                    }
                }
            }
        }
        // Close the stream — KWin destroys the virtual output on stream
        // close (the crate's manager sends Close and destroys the proxy).
        self.wl.read().await.close_stream().await;
        self.streams.write().await.clear();
        info!("[kwin-virtual] stream closed — virtual output removed, physical outputs restored");
    }

    async fn resize_capture_source(&self, width: u16, height: u16) -> Option<CaptureResizeOutcome> {
        // The virtual output is elastic: recreate it at the requested size
        // and the stream follows. zkde-screencast accepts ANY resolution —
        // this is the whole point of the strategy (no DRM mode list).
        //
        // Short-circuit when the size already matches: establish_for_client
        // creates (or reuses) the stream and request_initial_size follows
        // immediately with the client's request — recreating at the SAME
        // size would swap the output (close+create+rebind) for nothing.
        // The short-circuit's Unchanged effect means the caller must NOT
        // rebind, because rebinding destroys the live PipeWire stream —
        // which is bound to the zkde virtual output and tears it down with
        // it. With the physical output disabled by the layout guard, that
        // leaves ZERO enabled outputs and plasmashell falls back to its
        // placeholder screen: the session then streams all-zero buffers
        // forever (black screen on a healthy pipeline).
        {
            let cur = self.streams.read().await;
            if let Some(s) = cur.first()
                && s.width == width as u32
                && s.height == height as u32
            {
                // The live stream is already at this size (typical: the
                // establish_for_client pre-warm prediction hit). Record it
                // so the prediction survives this confirmation too — this
                // path returns before recreate_stream, which is otherwise
                // the only writer.
                *self
                    .last_output_size
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = Some((width, height));
                return Some(CaptureResizeOutcome {
                    width,
                    height,
                    effect: CaptureResizeEffect::Unchanged,
                });
            }
        }

        // Experiment D: switch the LIVE virtual output's mode in place via
        // kde-output-management-v2 custom modes — no destroy/recreate, so no
        // output add/remove, no PipeWire node rebind, no containment churn.
        // Kill-switchable via LAMCO_KWIN_INPLACE_MODE=0; needs an existing
        // live stream (per-connection resize only; establish still creates).
        // Every non-Applied outcome falls back to the recreate path below.
        if in_place_mode_change_enabled() && !self.streams.read().await.is_empty() {
            match self
                .wl
                .read()
                .await
                .try_change_mode_in_place(width, height)
                .await
            {
                ModeChangeOutcome::Applied => {
                    let node_id = self
                        .streams
                        .read()
                        .await
                        .first()
                        .map(|s| s.node_id)
                        .unwrap_or(0);
                    *self
                        .last_output_size
                        .lock()
                        .unwrap_or_else(|p| p.into_inner()) = Some((width, height));
                    *self.streams.write().await = vec![StreamInfo {
                        node_id,
                        width: width as u32,
                        height: height as u32,
                        position_x: 0,
                        position_y: 0,
                    }];
                    info!(
                        "[kwin-virtual] in-place mode change applied: {}x{} on existing stream (node {})",
                        width, height, node_id
                    );
                    return Some(CaptureResizeOutcome {
                        width,
                        height,
                        effect: CaptureResizeEffect::RelaidOutInPlace,
                    });
                }
                outcome => {
                    info!(
                        "[kwin-virtual] in-place mode change to {}x{} not applied ({outcome:?}) — falling back to recreate",
                        width, height
                    );
                }
            }
        }

        match self.recreate_stream(width, height).await {
            Ok(node) => {
                // The source was actually recreated: report the new node so
                // the caller rebinds the capture pipeline to it.
                Some(CaptureResizeOutcome {
                    width,
                    height,
                    effect: CaptureResizeEffect::Recreated { node_id: node },
                })
            }
            Err(_e) => {
                tracing::warn!(
                    "[kwin-virtual] resize to {width}x{height} failed: {_e} — keeping current stream"
                );
                let cur = self.streams.read().await;
                cur.first().map(|s| CaptureResizeOutcome {
                    width: s.width as u16,
                    height: s.height as u16,
                    effect: CaptureResizeEffect::Unchanged,
                })
            }
        }
    }

    fn clipboard_source(&self) -> ClipboardSource {
        ClipboardSource::None
    }

    // Clipboard comes from the composed libei session (Wayland data-control
    // via wl-clipboard-rs) — the same provider the standalone libei
    // strategy uses. clipboard_source() stays None: this path supplies the
    // provider through build_clipboard(), not the Portal/Mutter/DataControl
    // handles that enum carries.
    #[cfg(feature = "wl-clipboard")]
    async fn build_clipboard(
        &self,
        _portal_fallback: Option<crate::session::strategy::ClipboardComponents>,
        rate_limit_ms: u64,
    ) -> Option<std::sync::Arc<dyn crate::clipboard::provider::ClipboardProvider>> {
        self.libei
            .build_clipboard(_portal_fallback, rate_limit_ms)
            .await
    }
}

// NOTE: no adapter needed — create_session_concrete guarantees the concrete
// libei handle type for direct input delegation.

/// Session strategy: KWin zkde-screencast virtual output.
pub struct KwinVirtualStrategy {
    /// Token manager for the libei restore token (input consent).
    token_manager: Option<Arc<crate::session::token_manager::Tokens>>,
}

impl KwinVirtualStrategy {
    pub fn new(token_manager: Option<Arc<crate::session::token_manager::Tokens>>) -> Self {
        Self { token_manager }
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
        // plugin loads, so a pre-flight probe is unreliable).
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

        // Input: reuse the libei machinery (Portal RemoteDesktop + EIS,
        // restore token, persistent event consumer) — but INPUT-ONLY:
        // kwin-virtual provides its own video via zkde-screencast. Attaching
        // the libei ScreenCast here would give xdp-kde a monitor dependency
        // that kills the whole RemoteDesktop session the moment the output
        // guard disables the physical output (Session.Closed → ConnectToEIS
        // fails → dead keyboard/mouse for the client).
        let libei_strategy =
            crate::session::strategies::libei::LibeiStrategy::new(None, self.token_manager.clone());
        let libei_impl = libei_strategy.create_session_concrete(false).await?;

        // NOTE: the output-layout guard is NOT engaged here. It engages on
        // establish_for_client (per connection) and drops on
        // release_after_client — engaging at session creation would blank
        // the console for the entire server lifetime, hiding the one-time
        // input-consent dialog from the user (the dialog must be visible
        // on the console).
        let handle = Arc::new(KwinVirtualSessionHandle::new(libei_impl));
        Ok(handle as Arc<dyn SessionHandle>)
    }

    async fn cleanup(&self, _session: &dyn SessionHandle) -> Result<()> {
        // The layout guard is owned by the HANDLE and released on
        // release_after_client; nothing to restore here.
        info!("[kwin-virtual] session cleanup complete");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! kwin-virtual strategy unit tests.
    //!
    //! The Wayland object layer can't be exercised off-compositor. The
    //! pure-logic seams — the kscreen output parser (with its exact-name
    //! exclusion rule) and the stream-event state machine — moved to the
    //! hyperv-rdp-extras crate and are tested there (session module). The
    //! tests below keep the fork-side regression coverage: the parser tests
    //! exercise the SAME crate function through this module's re-exports,
    //! including the real-world Hyper-V sample that pins the exact-name
    //! exclusion against hyperv_drm's `Virtual-1`.

    use super::*;

    /// Real-world connector naming on Hyper-V: hyperv_drm's
    /// connector is named `Virtual-1` and MUST be managed (disabled) by the
    /// guard; our zkde output is `Virtual-lamco` and MUST be excluded —
    /// the exclusion is exact-name, never a `Virtual-` prefix (a prefix
    /// match would skip the DRM output and leave a two-screen layout that
    /// breaks pointer mapping). This sample mirrors that machine's output.
    const KSCREEN_TWO_OUTPUTS: &str = "Output: 1 Virtual-1\n        enabled\n        connected\n        priority 1\n        Unknown\n        Modes:  1:1024x768@60!  2:1920x1080@60*  3:1600x1200@60 \n        Geometry: 0,0 1920x1080\n        Scale: 1\nOutput: 2 Virtual-lamco\n        enabled\n        connected\n        priority 2\n        Unknown\n        Modes:  25:1920x1200@60*! \n        Geometry: 1920,0 1920x1200\n        Scale: 1\n";

    #[test]
    fn test_parser_hyperv_drm_output_is_managed() {
        // hyperv_drm's `Virtual-1` is a PHYSICAL
        // output here — it must appear (be disabled by the guard), not be
        // excluded by a Virtual- prefix.
        let names =
            parse_enabled_physical_outputs(KSCREEN_TWO_OUTPUTS, VIRTUAL_OUTPUT_KSCREEN_NAME);
        assert!(
            names.contains(&"Virtual-1".to_string()),
            "hyperv_drm's Virtual-1 must be managed; got {names:?}"
        );
    }

    #[test]
    fn test_parser_excludes_own_virtual_output() {
        let names =
            parse_enabled_physical_outputs(KSCREEN_TWO_OUTPUTS, VIRTUAL_OUTPUT_KSCREEN_NAME);
        assert!(
            !names.contains(&"Virtual-lamco".to_string()),
            "our own virtual output must NOT be disabled; got {names:?}"
        );
    }

    #[test]
    fn test_parser_single_drm_output() {
        let text = "Output: 1 Virtual-1\n        enabled\n        connected\n        Geometry: 0,0 1920x1080\n";
        let names = parse_enabled_physical_outputs(text, VIRTUAL_OUTPUT_KSCREEN_NAME);
        assert_eq!(names, vec!["Virtual-1".to_string()]);
    }

    #[test]
    fn test_parser_skips_disabled_outputs() {
        // A disabled output (e.g. the guard's own prior work, or a DPMS-off
        // monitor) must not be collected — enabling it later is harmless but
        // re-disable bookkeeping relies on the list being exactly "currently
        // enabled".
        let text = "Output: 1 Virtual-1\n        enabled\nOutput: 2 HDMI-A-1\n        disabled\n";
        let names = parse_enabled_physical_outputs(text, VIRTUAL_OUTPUT_KSCREEN_NAME);
        assert_eq!(names, vec!["Virtual-1".to_string()]);
    }

    #[test]
    fn test_parser_real_connector_names() {
        // Bare-metal KDE naming (DP-1/HDMI-A-1) — the common case.
        let text = "Output: 1 DP-1\n        enabled\nOutput: 2 HDMI-A-1\n        enabled\nOutput: 3 Virtual-lamco\n        enabled\n";
        let names = parse_enabled_physical_outputs(text, VIRTUAL_OUTPUT_KSCREEN_NAME);
        assert_eq!(names, vec!["DP-1".to_string(), "HDMI-A-1".to_string()]);
    }

    #[test]
    fn test_parser_empty_and_garbage_input() {
        assert!(parse_enabled_physical_outputs("", VIRTUAL_OUTPUT_KSCREEN_NAME).is_empty());
        assert!(
            parse_enabled_physical_outputs(
                "random noise\nno outputs here\n",
                VIRTUAL_OUTPUT_KSCREEN_NAME
            )
            .is_empty()
        );
        // "enabled" without a preceding Output block is ignored.
        assert!(
            parse_enabled_physical_outputs("enabled\nenabled\n", VIRTUAL_OUTPUT_KSCREEN_NAME)
                .is_empty()
        );
    }

    #[test]
    fn test_parser_no_trailing_newline() {
        // The final block flush must not require a trailing newline after
        // the last block — kscreen output shape varies.
        let text = "Output: 1 Virtual-1\n        enabled";
        let names = parse_enabled_physical_outputs(text, VIRTUAL_OUTPUT_KSCREEN_NAME);
        assert_eq!(names, vec!["Virtual-1".to_string()]);
    }
}
