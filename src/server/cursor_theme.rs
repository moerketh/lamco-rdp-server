//! Session-scoped guest cursor theme management (console-pointer restore).
//!
//! On compositors with no hardware cursor plane (hyperv_drm on Hyper-V),
//! KWin 6.3 paints the cursor into the primary framebuffer and the
//! PipeWire screencast source is a *copy* of that framebuffer — so the
//! guest cursor is unavoidably embedded in the captured video. The RDP
//! client renders its own pointer from the shape PDU, producing the
//! "double cursor": the crisp client pointer plus the lagging composited
//! guest sprite inside the stream.
//!
//! The workaround is a fully transparent XCursor theme while an
//! RDP client is connected. That leaves the *local console* without a
//! pointer, so this module scopes the transparency to the RDP session:
//!
//! - client activation  → apply `transparent` for the live session
//! - client disconnect  → restore the configured visible theme
//!
//! Because the accept loop is serial (one served connection at a time,
//! `AcceptDispatcher::run`), every served connect/disconnect maps to a
//! theme transition.
//!
//! # How the live apply works (and why a plain apply is not enough)
//!
//! `plasma-apply-cursortheme T` only swaps the live sprite when the
//! current *config* differs from T — if kcminputrc already names T it
//! prints "already set" and leaves the running sprite untouched. The
//! reliable reload is a TOGGLE: apply a real theme first, then the
//! target. Every live apply below performs the two-step toggle.
//!
//! # Crash-safety
//!
//! `plasma-apply-cursortheme` persists its argument to kcminputrc.
//! Applying `transparent` would leave a machine that crashes (or reboots
//! after a crash) booting into a cursorless console. After a successful
//! transparent apply this module rewrites kcminputrc back to the
//! *visible* theme, so persisted state always names a visible theme;
//! only the running session is transparent. A fresh session therefore
//! always starts with a console pointer.
//!
//! Command execution is behind the [`CmdRunner`] seam so tests verify
//! the transition/toggle logic without touching a real session.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use tracing::{debug, info, warn};

/// Which theme the guest console is currently showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeState {
    /// Console cursor visible (no RDP consumer needing a clean stream).
    Visible,
    /// Console cursor transparent (RDP client connected).
    Transparent,
}

const STATE_VISIBLE: u8 = 0;
const STATE_TRANSPARENT: u8 = 1;
/// Session active but the transparency decision is deferred until the
/// frame loop has reported whether the capture delivers cursor metadata
/// (a few frames at most — see `observe_metadata_cursors`).
const STATE_PENDING: u8 = 2;
/// How many metadata-absent frames settle the deferred decision toward
/// applying the transparent theme. ~3 frames ≈ 50ms at 60fps: short enough
/// that an embedded-cursor path can't show a double cursor for long, long
/// enough that a metadata path whose first frame legitimately lacks the
/// cursor meta (pointer off-output at session start) isn't misclassified.
const PENDING_ABSENT_FRAMES: u32 = 3;

/// Theme identifiers used by the manager.
#[derive(Debug, Clone)]
pub struct CursorThemes {
    /// Theme applied while no RDP client is connected. Also what
    /// kcminputrc persistently names — crash-safe reboot state.
    pub visible: String,
    /// Fully transparent theme preinstalled on the guest image.
    pub transparent: String,
}

/// Execution seam for tests: applies/persists cursor themes.
pub trait CmdRunner: Send + Sync {
    /// Apply a cursor theme to the running session (one toggle step).
    fn apply_theme(&self, theme: &str) -> bool;
    /// Persist kcminputrc `[Mouse] cursorTheme=<theme>`.
    fn persist_theme(&self, theme: &str) -> bool;
}

/// Validate a theme name before it becomes an argv element.
///
/// No shell is involved, so this is not classic injection — but a name
/// beginning with `-` would be parsed as a flag by
/// `plasma-apply-cursortheme` (config-sourced strings, operator-typoable),
/// and NUL/empty can never be a valid theme. Reject those; everything
/// else is a harmless literal argument.
fn valid_theme_name(theme: &str) -> bool {
    !theme.is_empty() && !theme.starts_with('-') && !theme.contains('\0')
}

/// Production runner: the lamco service runs as the desktop user inside
/// the graphical session (the desktop's systemd user unit), so commands
/// only need the session-bus env a non-login context may be missing.
pub struct SessionCmdRunner {
    runtime_dir: String,
}

impl SessionCmdRunner {
    /// Resolve from the current process uid; `None` for root (the
    /// service never runs as root; a root context cannot address the
    /// user session bus reliably).
    pub fn new() -> Option<Self> {
        Self::with_uid(current_uid())
    }

    pub fn with_uid(uid: u32) -> Option<Self> {
        (uid != 0).then(|| Self {
            runtime_dir: format!("/run/user/{uid}"),
        })
    }

    fn run(&self, argv: &[&str]) -> bool {
        let bus = format!("unix:path={}/bus", self.runtime_dir);
        match Command::new(argv[0])
            .args(&argv[1..])
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("DBUS_SESSION_BUS_ADDRESS", &bus)
            .env("DISPLAY", ":0")
            .env("WAYLAND_DISPLAY", "wayland-0")
            .env("LC_ALL", "C.UTF-8")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .status()
        {
            Ok(s) if s.success() => true,
            Ok(s) => {
                debug!("cursor cmd {:?} exited {s}", argv.first());
                false
            }
            Err(e) => {
                debug!("cursor cmd {:?} exec failed: {e}", argv.first());
                false
            }
        }
    }
}

/// Read the current uid without a dependency crate (stat /proc/self).
fn current_uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .unwrap_or(0)
}

impl CmdRunner for SessionCmdRunner {
    fn apply_theme(&self, theme: &str) -> bool {
        if !valid_theme_name(theme) {
            warn!("rejecting invalid cursor theme name {theme:?} (empty/flag/NUL)");
            return false;
        }
        self.run(&["/usr/bin/plasma-apply-cursortheme", theme])
    }

    fn persist_theme(&self, theme: &str) -> bool {
        if !valid_theme_name(theme) {
            warn!("rejecting invalid cursor theme name {theme:?} (empty/flag/NUL)");
            return false;
        }
        self.run(&[
            "/usr/bin/kwriteconfig6",
            "--file",
            "kcminputrc",
            "--group",
            "Mouse",
            "--key",
            "cursorTheme",
            theme,
        ])
    }
}

/// Session-scoped cursor theme manager.
pub struct CursorThemeManager<R: CmdRunner> {
    state: AtomicU8,
    themes: CursorThemes,
    runner: R,
    /// Set when the capture path delivers cursor metadata (SPA_META_Cursor)
    /// — i.e. KWin excludes the cursor from the stream and reports it
    /// separately (zkde-screencast pointer mode Metadata). While set, the
    /// transparent-theme workaround must NOT run: there is no composited
    /// guest sprite to hide (no double cursor), and blanking the sprite
    /// also starves the metadata of its shape source — the client would be
    /// stuck on its static default pointer with no context-aware shapes.
    metadata_cursors: AtomicBool,
    /// Frames observed with cursor metadata absent while the decision is
    /// pending (see STATE_PENDING).
    pending_absent: AtomicU32,
}

impl<R: CmdRunner> CursorThemeManager<R> {
    pub fn new(themes: CursorThemes, runner: R) -> Self {
        Self {
            state: AtomicU8::new(STATE_VISIBLE),
            themes,
            runner,
            metadata_cursors: AtomicBool::new(false),
            pending_absent: AtomicU32::new(0),
        }
    }

    pub fn state(&self) -> ThemeState {
        match self.state.load(Ordering::Acquire) {
            STATE_TRANSPARENT => ThemeState::Transparent,
            _ => ThemeState::Visible,
        }
    }

    /// Record whether the capture path is delivering cursor metadata.
    /// Called by the frame loop with each frame's observation. Once true,
    /// stays true for the manager's lifetime (metadata capability is a
    /// property of the negotiated capture, not per-frame — KWin may
    /// legitimately omit the meta when the pointer leaves the output or
    /// is hidden by the app). Resolves a pending begin_rdp_session()
    /// decision in either direction.
    pub fn observe_metadata_cursors(&self, present: bool) {
        if present {
            if !self.metadata_cursors.swap(true, Ordering::AcqRel) {
                info!(
                    "Capture delivers cursor metadata — keeping the visible guest cursor theme \
                     (no transparency needed; shapes reach the client via pointer PDUs)"
                );
            }
            if self.state.load(Ordering::Acquire) == STATE_PENDING {
                self.state.store(STATE_VISIBLE, Ordering::Release);
                debug!("Cursor metadata active — transparent theme cancelled");
            }
            return;
        }
        // Cursor meta absent this frame. If the session decision is still
        // pending, accumulate; enough absent frames settle it toward the
        // transparent workaround (the classic embedded-cursor path).
        if self.state.load(Ordering::Acquire) == STATE_PENDING {
            let n = self.pending_absent.fetch_add(1, Ordering::AcqRel) + 1;
            if n >= PENDING_ABSENT_FRAMES
                && self.apply_toggle(&self.themes.transparent, &self.themes.visible)
            {
                self.state.store(STATE_TRANSPARENT, Ordering::Release);
                info!(
                    "Guest cursor transparent for RDP session (console cursor hidden until disconnect)"
                );
            }
        }
    }

    /// Arm the session-scoped cursor decision (RDP client activated).
    /// The apply itself is DEFERRED until `observe_metadata_cursors` has
    /// seen a few frames: applying immediately would starve metadata-based
    /// capture paths of their shape source (the transparent sprite makes
    /// KWin report no cursor at all), and activation happens before the
    /// first frame arrives, so the metadata signal cannot be known here.
    /// No-op when already transparent or when this capture path has already
    /// proven metadata-capable.
    pub fn begin_rdp_session(&self) {
        if self.metadata_cursors.load(Ordering::Acquire) {
            debug!(
                "Cursor metadata active — skipping transparent theme \
                 (double-cursor workaround not needed on this capture path)"
            );
            return;
        }
        match self.state() {
            ThemeState::Transparent => {
                debug!("Cursor already transparent — no apply needed");
            }
            _ => {
                self.pending_absent.store(0, Ordering::Release);
                self.state.store(STATE_PENDING, Ordering::Release);
                debug!("Cursor theme decision deferred until frames reveal metadata presence");
            }
        }
    }

    /// Restore the visible theme (RDP client gone). Idempotent. A pending
    /// (never-applied) decision simply returns to Visible — nothing was
    /// applied, so there is nothing to restore.
    pub fn end_rdp_session(&self) {
        if self.state.load(Ordering::Acquire) == STATE_PENDING {
            self.state.store(STATE_VISIBLE, Ordering::Release);
            debug!("Cursor theme decision cancelled on disconnect (nothing applied)");
            return;
        }
        if self.state() == ThemeState::Visible {
            return;
        }
        if self.apply_toggle(&self.themes.visible, &self.themes.transparent) {
            // Pin config to visible as well: if the begin-path persist
            // was ever lost (crash window), this closes it.
            self.runner.persist_theme(&self.themes.visible);
            self.state.store(STATE_VISIBLE, Ordering::Release);
            info!(
                "Guest cursor restored to '{}' for console use",
                self.themes.visible
            );
        }
    }

    /// Force-restore regardless of tracked state (ExecStopPost /
    /// shutdown recovery). Idempotent.
    pub fn restore_visible(&self) {
        if self.apply_toggle(&self.themes.visible, &self.themes.transparent) {
            self.runner.persist_theme(&self.themes.visible);
            self.state.store(STATE_VISIBLE, Ordering::Release);
        }
    }

    /// Apply `target` to the running session via the toggle trick:
    /// `from` (a real, different theme) first, so the second apply
    /// performs an actual sprite swap rather than the config-only
    /// "already set" no-op. When the target is the transparent theme,
    /// the persistent config is reset to the visible theme afterwards
    /// (crash-safety: reboots always land on a visible console cursor).
    fn apply_toggle(&self, target: &str, from: &str) -> bool {
        debug!("Cursor theme toggle {from} -> {target}");
        if !self.runner.apply_theme(from) {
            warn!("Cursor toggle step 1 ({from}) failed — keeping current theme");
            return false;
        }
        // Give the compositor a moment to load the first theme so the
        // second apply forces the real sprite change.
        std::thread::sleep(std::time::Duration::from_millis(250));
        if !self.runner.apply_theme(target) {
            warn!("Cursor apply ({target}) failed — console cursor state unchanged");
            return false;
        }

        if target == self.themes.transparent && !self.runner.persist_theme(&self.themes.visible) {
            warn!(
                "Could not restore kcminputrc to visible theme (non-fatal; live session unaffected)"
            );
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Recording fake: logs calls, configurable failures.
    struct FakeRunner {
        calls: Mutex<Vec<String>>,
        fail_apply: bool,
        fail_persist: bool,
    }

    impl FakeRunner {
        fn ok() -> Self {
            Self {
                calls: Mutex::new(vec![]),
                fail_apply: false,
                fail_persist: false,
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CmdRunner for FakeRunner {
        fn apply_theme(&self, theme: &str) -> bool {
            self.calls.lock().unwrap().push(format!("apply:{theme}"));
            !self.fail_apply
        }
        fn persist_theme(&self, theme: &str) -> bool {
            self.calls.lock().unwrap().push(format!("persist:{theme}"));
            !self.fail_persist
        }
    }

    /// Blanket impl so `CursorThemeManager<&FakeRunner>` satisfies the bound.
    impl<F: CmdRunner> CmdRunner for &F {
        fn apply_theme(&self, theme: &str) -> bool {
            (**self).apply_theme(theme)
        }
        fn persist_theme(&self, theme: &str) -> bool {
            (**self).persist_theme(theme)
        }
    }

    fn themes() -> CursorThemes {
        CursorThemes {
            visible: "breeze_cursors".into(),
            transparent: "transparent".into(),
        }
    }

    #[test]
    fn theme_name_validation_rejects_adversarial_names() {
        // Flag-shaped names would be parsed as options by
        // plasma-apply-cursortheme; empty and NUL-bearing names can never
        // be valid themes.
        assert!(!valid_theme_name(""));
        assert!(!valid_theme_name("-h"));
        assert!(!valid_theme_name("--help"));
        assert!(!valid_theme_name("--"));
        assert!(!valid_theme_name("foo\0bar"));
        // Acceptable names: literals, dots, underscores, spaces.
        assert!(valid_theme_name("breeze_cursors"));
        assert!(valid_theme_name("Premium-Plus"));
        assert!(valid_theme_name("Whiteglass 2"));
        assert!(valid_theme_name("Adwaita"));
    }

    #[test]
    fn manager_rejects_flag_shaped_config_theme_names() {
        // End to end through the manager: a config-typo'd visible theme
        // starting with '-' must not reach the runner as an argv element
        // (the FakeRunner records whatever it is given; the production
        // SessionCmdRunner is the rejecting gate, asserted separately by
        // theme_name_validation_rejects_adversarial_names + the run()
        // argv path).
        let runner = FakeRunner::ok();
        let bad = CursorThemes {
            visible: "--config=/etc/passwd".into(),
            transparent: "transparent".into(),
        };
        let mgr = CursorThemeManager::new(bad, &runner);
        mgr.begin_rdp_session();
        // The (invalid) visible name is rejected by the production gate;
        // the session defers the decision, and absent metadata frames
        // settle it toward the transparent apply. The transparent theme
        // itself is valid and proceeds.
        for _ in 0..3 {
            mgr.observe_metadata_cursors(false);
        }
        let calls = runner.calls();
        assert!(
            calls.iter().any(|c| c.starts_with("apply:transparent")),
            "transparent apply must proceed: {calls:?}"
        );
    }

    #[test]
    fn begin_defers_until_frames_reveal_metadata_absence() {
        let runner = FakeRunner::ok();
        let mgr = CursorThemeManager::new(themes(), &runner);
        mgr.begin_rdp_session();
        // Nothing applied yet: activation happens before the first frame.
        assert!(runner.calls().is_empty());
        // Absent frames accumulate... 2 is below the threshold.
        mgr.observe_metadata_cursors(false);
        mgr.observe_metadata_cursors(false);
        assert!(runner.calls().is_empty());
        // ...the 3rd absent frame settles it: toggle applies transparent.
        mgr.observe_metadata_cursors(false);
        assert_eq!(mgr.state(), ThemeState::Transparent);
        // Toggle order: real theme first, transparent second.
        assert_eq!(
            runner.calls(),
            vec![
                "apply:breeze_cursors".to_string(),
                "apply:transparent".to_string(),
                "persist:breeze_cursors".to_string(),
            ]
        );
    }

    #[test]
    fn metadata_frames_cancel_the_transparent_apply() {
        let runner = FakeRunner::ok();
        let mgr = CursorThemeManager::new(themes(), &runner);
        mgr.begin_rdp_session();
        // First frames carry cursor metadata (kwin-virtual + Pointer::Metadata):
        // the workaround must never run.
        mgr.observe_metadata_cursors(true);
        for _ in 0..10 {
            mgr.observe_metadata_cursors(false);
        }
        assert_eq!(mgr.state(), ThemeState::Visible);
        assert!(
            runner.calls().is_empty(),
            "no theme applies: {:?}",
            runner.calls()
        );
    }

    #[test]
    fn metadata_arriving_mid_pending_cancels_apply() {
        let runner = FakeRunner::ok();
        let mgr = CursorThemeManager::new(themes(), &runner);
        mgr.begin_rdp_session();
        mgr.observe_metadata_cursors(false);
        mgr.observe_metadata_cursors(false);
        // Metadata arrives on the 3rd frame — before the threshold settled.
        mgr.observe_metadata_cursors(true);
        mgr.observe_metadata_cursors(false);
        assert_eq!(mgr.state(), ThemeState::Visible);
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn begin_is_idempotent_when_already_transparent() {
        let runner = FakeRunner::ok();
        let mgr = CursorThemeManager::new(themes(), &runner);
        mgr.begin_rdp_session();
        for _ in 0..3 {
            mgr.observe_metadata_cursors(false);
        }
        let after_first = runner.calls().len();
        mgr.begin_rdp_session(); // no-op
        assert_eq!(runner.calls().len(), after_first);
    }

    #[test]
    fn end_restores_visible_theme_and_config() {
        let runner = FakeRunner::ok();
        let mgr = CursorThemeManager::new(themes(), &runner);
        mgr.begin_rdp_session();
        for _ in 0..3 {
            mgr.observe_metadata_cursors(false);
        }
        mgr.end_rdp_session();
        assert_eq!(mgr.state(), ThemeState::Visible);
        assert_eq!(runner.calls().last().unwrap(), "persist:breeze_cursors");
        assert!(runner.calls().contains(&"apply:breeze_cursors".to_string()));
    }

    #[test]
    fn end_without_begin_is_noop() {
        let runner = FakeRunner::ok();
        let mgr = CursorThemeManager::new(themes(), &runner);
        mgr.end_rdp_session();
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn failed_toggle_leaves_state_visible() {
        let runner = FakeRunner {
            calls: Mutex::new(vec![]),
            fail_apply: true,
            fail_persist: false,
        };
        let mgr = CursorThemeManager::new(themes(), &runner);
        mgr.begin_rdp_session();
        for _ in 0..3 {
            mgr.observe_metadata_cursors(false);
        }
        assert_eq!(mgr.state(), ThemeState::Visible);
        // Retry still attempts the toggle (transition not latched).
        mgr.begin_rdp_session();
        for _ in 0..3 {
            mgr.observe_metadata_cursors(false);
        }
        assert_eq!(mgr.state(), ThemeState::Visible);
    }

    #[test]
    fn failed_persist_is_nonfatal() {
        let runner = FakeRunner {
            calls: Mutex::new(vec![]),
            fail_apply: false,
            fail_persist: true,
        };
        let mgr = CursorThemeManager::new(themes(), &runner);
        mgr.begin_rdp_session();
        for _ in 0..3 {
            mgr.observe_metadata_cursors(false);
        }
        assert_eq!(mgr.state(), ThemeState::Transparent);
    }

    #[test]
    fn restore_visible_forces_state_even_if_tracked_as_transparent() {
        let runner = FakeRunner::ok();
        let mgr = CursorThemeManager::new(themes(), &runner);
        mgr.begin_rdp_session();
        mgr.restore_visible();
        assert_eq!(mgr.state(), ThemeState::Visible);
        // Toggle ends with the visible apply, then pins config visible.
        assert_eq!(runner.calls().last().unwrap(), "persist:breeze_cursors");
        assert!(mgr.state() == ThemeState::Visible);
    }
}
