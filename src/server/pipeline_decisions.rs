//! Pure decision logic lifted out of the `start_pipeline` per-frame loop.
//!
//! These functions carry no `.await`, no I/O, and no shared-state borrows, so
//! they are unit-testable in isolation — unlike the same logic when it lived
//! inline inside the ~1900-line pipeline closure (see the architecture audit's
//! H4/M2 findings). The closure now calls these and keeps only the orchestration
//! (channel sends, encoder construction, logging) at the call sites.

use std::time::{Duration, Instant};

use crate::damage::DamageRegion;

/// Per-frame presentation timestamp for the H.264 encode path.
///
/// Prefers the PipeWire PTS (nanoseconds → milliseconds) when present;
/// otherwise synthesizes a monotonic timestamp from the sent-frame count and
/// the configured target FPS. `target_fps` is clamped to ≥1 to avoid a divide
/// by zero on a misconfigured `[video] target_fps = 0`.
pub(crate) fn compute_timestamp_ms(pts: u64, frames_sent: u64, target_fps: u32) -> u64 {
    if pts > 0 {
        pts / 1_000_000
    } else {
        let frame_interval_ms = 1000 / u64::from(target_fps.max(1));
        frames_sent * frame_interval_ms
    }
}

/// Fraction of the frame area covered by damage regions (0.0 when nothing
/// changed). Drives adaptive-FPS activity tracking and the latency governor.
pub(crate) fn compute_damage_ratio(regions: &[DamageRegion], width: u32, height: u32) -> f32 {
    if regions.is_empty() {
        return 0.0;
    }
    let frame_area = u64::from(width) * u64::from(height);
    if frame_area == 0 {
        return 0.0;
    }
    let damage_area: u64 = regions.iter().map(DamageRegion::area).sum();
    damage_area as f32 / frame_area as f32
}

/// Decide whether the pipeline should skip encoding this frame because the
/// client asked us to stop (`SuppressOutput { desktop_rect: None }`, e.g.
/// mstsc minimized).
///
/// Policy (matching IronRDP's documented guidance for this handle):
/// - Never gate before the client has received its first frame: some clients
///   (notably mstsc) raise SuppressOutput during the connect handshake, before
///   their display surface exists — gating there leaves a half-initialized
///   surface that doesn't recover on un-suppress (visible as a frozen desktop
///   on first connect).
/// - Debounce transient flaps: engage only once the flag has been steady
///   `true` for `ENGAGE_AFTER` (some clients pulse the PDU under wire
///   pressure), and release immediately when it clears — a returning client
///   must get frames at once, not after another delay.
/// - `None` (no shared flag) never skips: the gate is inactive and the
///   pipeline encodes unconditionally.
pub(crate) fn should_skip_for_suppress(
    suppressed: Option<&std::sync::atomic::AtomicBool>,
    suppressed_since: Option<Instant>,
    frames_sent: u64,
    now: Instant,
) -> bool {
    const ENGAGE_AFTER: Duration = Duration::from_millis(1000);
    const FIRST_FRAME_GRACE: u64 = 1;

    let Some(flag) = suppressed else {
        return false;
    };
    // First-frame grace: the client hasn't presented anything yet, so there is
    // no backlog to avoid and gating can only break handshake-time suppress.
    if frames_sent < FIRST_FRAME_GRACE {
        return false;
    }
    match (
        flag.load(std::sync::atomic::Ordering::Relaxed),
        suppressed_since,
    ) {
        (false, _) => false,
        // Engaged already; stay gated until the flag clears (no re-delay).
        (true, Some(since)) => now.duration_since(since) >= ENGAGE_AFTER,
        // Flag just observed high with no recorded onset — record happens in
        // the caller; treat as not-yet-engaged so the debounce interval runs.
        (true, None) => false,
    }
}

#[cfg(test)]
mod suppress_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn now_after(secs: u64) -> Instant {
        Instant::now()
            .checked_add(Duration::from_secs(secs))
            .expect("representable")
    }

    #[test]
    fn no_flag_never_skips() {
        assert!(!should_skip_for_suppress(None, None, 100, Instant::now()));
    }

    #[test]
    fn first_frame_grace_beats_suppress_during_handshake() {
        let flag = AtomicBool::new(true);
        let t0 = Instant::now();
        // Zero frames sent: even a steady suppress must not gate.
        assert!(!should_skip_for_suppress(
            Some(&flag),
            Some(t0),
            0,
            now_after(10)
        ));
    }

    #[test]
    fn engages_only_after_steady_interval() {
        let flag = AtomicBool::new(true);
        let t0 = Instant::now();
        // 0.5s in: below the 1s debounce → no skip.
        let half = t0
            .checked_add(Duration::from_millis(500))
            .expect("representable");
        assert!(!should_skip_for_suppress(Some(&flag), Some(t0), 5, half));
        // 1s in: engaged.
        let full = t0
            .checked_add(Duration::from_millis(1000))
            .expect("representable");
        assert!(should_skip_for_suppress(Some(&flag), Some(t0), 5, full));
    }

    #[test]
    fn releases_immediately_when_flag_clears() {
        let flag = AtomicBool::new(false);
        let t0 = Instant::now();
        assert!(!should_skip_for_suppress(
            Some(&flag),
            Some(t0),
            5,
            now_after(10)
        ));
    }

    #[test]
    fn unrecorded_onset_does_not_engage() {
        let flag = AtomicBool::new(true);
        // High flag but the caller hasn't recorded an onset yet → not engaged.
        assert!(!should_skip_for_suppress(
            Some(&flag),
            None,
            5,
            Instant::now()
        ));
    }
}

/// Resolve the AVC444-vs-AVC420 codec decision from the configured preference,
/// the client's advertised AVC444 capability, and the `[egfx] avc444_enabled`
/// config flag. Returns the decision plus the human-readable reason the caller
/// logs, so the branch-by-branch logging is preserved without keeping the
/// decision tree inline in the pipeline loop.
pub(crate) fn resolve_avc444_enabled(
    codec_pref: &str,
    client_supports_avc444: bool,
    config_avc444_enabled: bool,
) -> (bool, &'static str) {
    match codec_pref {
        "avc420" => (false, "Codec preference: AVC420 forced by config"),
        "avc444" => {
            if client_supports_avc444 && config_avc444_enabled {
                (true, "Codec preference: AVC444 requested and supported")
            } else if !client_supports_avc444 {
                (
                    false,
                    "Codec preference: AVC444 requested but client doesn't support it, using AVC420",
                )
            } else {
                (
                    false,
                    "Codec preference: AVC444 requested but disabled in config, using AVC420",
                )
            }
        }
        // "auto" or unrecognized: use the best mutually-available codec.
        _ => {
            if config_avc444_enabled && client_supports_avc444 {
                (
                    true,
                    "Codec preference: auto → AVC444 (client supports, enabled in config)",
                )
            } else if !config_avc444_enabled {
                (
                    false,
                    "Codec preference: auto → AVC420 (AVC444 disabled in config)",
                )
            } else {
                (
                    false,
                    "Codec preference: auto → AVC420 (client doesn't support AVC444)",
                )
            }
        }
    }
}

/// Result of the L2 stress-detector evaluation.
pub(crate) struct StressIdrEval {
    /// Whether an early IDR should be requested to break the P-slice chain.
    pub should_trigger: bool,
    /// Drop rate over the window (kept for the caller's diagnostic log).
    pub drop_rate: f64,
}

/// Decide whether sustained frame drops warrant an early IDR.
///
/// An early IDR is requested only when the drop rate over the rolling window
/// exceeds the threshold AND the sample is meaningful (≥5 frames) AND the
/// post-trigger cooldown has elapsed AND enough time has passed since the last
/// IDR — the conjunction that stops mstsc's decoder from desyncing on a long
/// arrival-delayed P-slice chain without flapping.
pub(crate) fn evaluate_stress_idr_trigger(
    dropped_in_window: u64,
    sent_in_window: u64,
    drop_rate_threshold: f64,
    cooldown_elapsed_ms: u64,
    cooldown_ms: u64,
    ms_since_last_idr: u64,
    min_idr_gap_ms: u64,
) -> StressIdrEval {
    let total_in_window = dropped_in_window + sent_in_window;
    let drop_rate = if total_in_window > 0 {
        dropped_in_window as f64 / total_in_window as f64
    } else {
        0.0
    };
    let should_trigger = drop_rate > drop_rate_threshold
        && total_in_window >= 5
        && cooldown_elapsed_ms > cooldown_ms
        && ms_since_last_idr > min_idr_gap_ms;
    StressIdrEval {
        should_trigger,
        drop_rate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_prefers_pts_when_present() {
        // 5 ms expressed in nanoseconds.
        assert_eq!(compute_timestamp_ms(5_000_000, 999, 30), 5);
    }

    #[test]
    fn timestamp_synthesizes_from_frame_count_when_pts_zero() {
        // 30 fps → 33 ms interval; frame 10 → 330 ms.
        assert_eq!(compute_timestamp_ms(0, 10, 30), 330);
    }

    #[test]
    fn timestamp_clamps_zero_fps() {
        // target_fps 0 must not divide by zero; clamps to 1 fps (1000 ms/frame).
        assert_eq!(compute_timestamp_ms(0, 3, 0), 3000);
    }

    #[test]
    fn damage_ratio_empty_is_zero() {
        assert_eq!(compute_damage_ratio(&[], 1920, 1080), 0.0);
    }

    #[test]
    fn damage_ratio_full_frame_is_one() {
        let regions = [DamageRegion::new(0, 0, 1920, 1080)];
        assert!((compute_damage_ratio(&regions, 1920, 1080) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn damage_ratio_quarter_frame() {
        // A 960×540 region of a 1920×1080 frame is 1/4 of the area.
        let regions = [DamageRegion::new(0, 0, 960, 540)];
        assert!((compute_damage_ratio(&regions, 1920, 1080) - 0.25).abs() < 1e-6);
    }

    #[test]
    fn damage_ratio_guards_zero_area_frame() {
        let regions = [DamageRegion::new(0, 0, 10, 10)];
        assert_eq!(compute_damage_ratio(&regions, 0, 0), 0.0);
    }

    #[test]
    fn avc444_forced_off_by_config() {
        let (enabled, reason) = resolve_avc444_enabled("avc420", true, true);
        assert!(!enabled);
        assert!(reason.contains("AVC420 forced"));
    }

    #[test]
    fn avc444_requested_and_supported() {
        assert_eq!(resolve_avc444_enabled("avc444", true, true).0, true);
    }

    #[test]
    fn avc444_requested_but_client_unsupported() {
        let (enabled, reason) = resolve_avc444_enabled("avc444", false, true);
        assert!(!enabled);
        assert!(reason.contains("client doesn't support"));
    }

    #[test]
    fn avc444_requested_but_config_disabled() {
        let (enabled, reason) = resolve_avc444_enabled("avc444", true, false);
        assert!(!enabled);
        assert!(reason.contains("disabled in config"));
    }

    #[test]
    fn avc444_auto_picks_best_available() {
        assert_eq!(resolve_avc444_enabled("auto", true, true).0, true);
        assert_eq!(resolve_avc444_enabled("auto", false, true).0, false);
        assert_eq!(resolve_avc444_enabled("auto", true, false).0, false);
    }

    #[test]
    fn avc444_unrecognized_pref_behaves_as_auto() {
        // An unknown codec string falls through to the auto branch.
        assert_eq!(resolve_avc444_enabled("nonsense", true, true).0, true);
    }

    #[test]
    fn stress_triggers_on_sustained_drops() {
        // 8 dropped / 10 total = 0.8 drop rate > 0.5; sample ≥5; cooldown and
        // IDR gap both satisfied.
        let eval = evaluate_stress_idr_trigger(8, 2, 0.5, 2000, 1000, 2000, 1500);
        assert!(eval.should_trigger);
        assert!((eval.drop_rate - 0.8).abs() < 1e-6);
    }

    #[test]
    fn stress_holds_below_threshold() {
        // 4 dropped / 10 total = 0.4 < 0.5.
        assert!(!evaluate_stress_idr_trigger(4, 6, 0.5, 2000, 1000, 2000, 1500).should_trigger);
    }

    #[test]
    fn stress_holds_on_tiny_sample() {
        // 3 dropped / 3 total = 1.0 drop rate but sample < 5.
        assert!(!evaluate_stress_idr_trigger(3, 0, 0.5, 2000, 1000, 2000, 1500).should_trigger);
    }

    #[test]
    fn stress_holds_during_cooldown() {
        // High drop rate + sample, but cooldown has not elapsed.
        assert!(!evaluate_stress_idr_trigger(8, 2, 0.5, 500, 1000, 2000, 1500).should_trigger);
    }

    #[test]
    fn stress_holds_when_recent_idr() {
        // High drop rate + sample + cooldown, but an IDR was just sent.
        assert!(!evaluate_stress_idr_trigger(8, 2, 0.5, 2000, 1000, 800, 1500).should_trigger);
    }

    #[test]
    fn stress_drop_rate_zero_on_empty_window() {
        let eval = evaluate_stress_idr_trigger(0, 0, 0.5, 2000, 1000, 2000, 1500);
        assert!(!eval.should_trigger);
        assert_eq!(eval.drop_rate, 0.0);
    }
}
