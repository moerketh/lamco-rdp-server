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
///
/// Note: sums region areas without deduplication — callers passing
/// overlapping regions get a ratio inflated above the true coverage (and
/// possibly >1.0). Feed this from a merged set (see `DamageAccumulator`)
/// when the ratio drives decisions.
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

/// Tracks damage regions from consumed-but-unsent frames ("debt") so no
/// region is ever lost when the latency governor skips or a send fails.
///
/// Compositor damage hints are one-shot: a skipped frame's regions must be
/// re-sent with a later one or the client keeps stale pixels forever.
///
/// Invariants:
/// - `absorb` REPLACES the debt (it does not extend it): the incoming set
///   already contains the prior debt, because the pipeline prepends debt to
///   each frame's fresh regions before the governor runs. Extending here is
///   what made the original inline code double the region count on every
///   consecutive skip (2ⁿ growth across a sub-threshold skip streak —
///   Interactive mode skips until `max_frame_delay_ms` elapses, easily 6-10
///   consecutive skips at 60 fps).
/// - The stored set is always merged (`merge_regions`) and hard-capped, so
///   the debt cannot grow without bound and `compute_damage_ratio` over
///   `take()`-ed output cannot double-count area.
pub(crate) struct DamageAccumulator {
    regions: Vec<DamageRegion>,
    cap: usize,
}

impl DamageAccumulator {
    /// Maximum number of rects retained as debt. The cap is never silently
    /// exceeded: when it binds, the whole debt is replaced by its bounding
    /// union — oversending the safe superset rather than dropping updates.
    pub(crate) const DEFAULT_CAP: usize = 1024;

    pub(crate) fn new() -> Self {
        Self {
            regions: Vec::new(),
            cap: Self::DEFAULT_CAP,
        }
    }

    /// Replace the debt with `send_set` (prior debt ∪ fresh regions), merged.
    /// Called on the skip/wait paths where the frame was consumed but will
    /// not be encoded.
    pub(crate) fn absorb(&mut self, send_set: Vec<DamageRegion>) {
        self.regions = Self::normalize(send_set, self.cap);
    }

    /// Take the entire debt, leaving the accumulator empty. The pipeline
    /// prepends the returned regions to the next encoded frame's set.
    pub(crate) fn take(&mut self) -> Vec<DamageRegion> {
        std::mem::take(&mut self.regions)
    }

    /// Drop all debt (e.g. on reconnect/resize where the coordinate space
    /// changed or the client will be fully re-initialized anyway).
    pub(crate) fn clear(&mut self) {
        self.regions.clear();
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.regions.len()
    }

    /// Merge overlapping/adjacent regions; if the merged set still exceeds
    /// the cap, collapse it to a single bounding union.
    fn normalize(mut regions: Vec<DamageRegion>, cap: usize) -> Vec<DamageRegion> {
        if regions.len() <= 1 {
            return regions;
        }
        regions = crate::damage::merge_regions(regions, 0);
        if regions.len() > cap
            && let Some(first) = regions.first()
        {
            let union = regions.iter().skip(1).fold(*first, |acc, r| acc.union(r));
            return vec![union];
        }
        regions
    }
}

impl Default for DamageAccumulator {
    fn default() -> Self {
        Self::new()
    }
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

/// Result of the compositor-hint trust evaluation.
pub(crate) struct CompositorTrustEval {
    /// Updated consecutive-high-divergence counter (feed back into the next call).
    pub new_consecutive_count: u32,
    /// Whether compositor damage hints should now be distrusted for the rest
    /// of this connection.
    pub should_distrust: bool,
}

/// Decide whether the periodic compositor-hint-vs-pixel-diff calibration
/// probe has diverged enough, consecutively enough, to stop trusting
/// compositor-supplied damage regions for the rest of this connection.
///
/// `divergence_pp` is the absolute delta (percentage points) between the
/// compositor-hint damage ratio and the pixel-diff damage ratio for one
/// probe sample. A single high-divergence sample does not distrust — only
/// `required_consecutive` samples in a row do, so one real full-screen
/// redraw landing on a probe frame can't misfire this. Any sample within
/// threshold resets the counter to 0 (consecutive, not cumulative).
pub(crate) fn evaluate_compositor_trust(
    divergence_pp: f32,
    consecutive_high_divergence: u32,
    threshold_pp: f32,
    required_consecutive: u32,
) -> CompositorTrustEval {
    let exceeds = divergence_pp.abs() > threshold_pp;
    let new_consecutive_count = if exceeds {
        consecutive_high_divergence + 1
    } else {
        0
    };
    CompositorTrustEval {
        new_consecutive_count,
        should_distrust: new_consecutive_count >= required_consecutive,
    }
}

/// Subtract the coverage of `covered` from `regions`, returning the parts of
/// `regions` NOT covered by `covered`.
///
/// Used by the damage-calibration probe: when the pixel-diff detector sees
/// changed pixels that the compositor's hints did not report, those missed
/// areas must still be sent to the client — the probe already advanced the
/// detector's reference frame, so anything not sent would be invisible to
/// every future diff (reference-ahead-of-client desync; persists until the
/// next periodic IDR).
///
/// Exact rectangle subtraction with axis-aligned splitting: each input region
/// is clipped against the covered area, producing up to 4 remainder rects per
/// covered intersection. Output is not merged — callers merge downstream
/// (`merge_regions` in the damage pipeline) or accept the coarse
/// fragmentation, which the tile-aligned inputs keep small in practice.
///
/// Worst case is multiplicative: each covering rect can split every surviving
/// piece into up to 4 bands, so a pathological `covered` set produces
/// O(4^|covered|) pieces per input region. A piece cap guards this: when it
/// binds, the region is returned un-subtracted (a conservative oversend of
/// its full area — the probe-union send set tolerates oversending, but never
/// dropping).
pub(crate) fn subtract_regions(
    regions: &[DamageRegion],
    covered: &[DamageRegion],
    frame_width: u32,
    frame_height: u32,
) -> Vec<DamageRegion> {
    /// Per-region cap on fragmentation pieces before falling back to the
    /// un-subtracted region. Generous for real workloads (compositor hints
    /// arrive tile-aligned and coarse), while bounding the exponential.
    const MAX_PIECES: usize = 256;

    // Fast paths: nothing to subtract from / by.
    if regions.is_empty() || covered.is_empty() {
        return regions.to_vec();
    }
    // A covered region spanning the whole frame erases everything.
    let full_coverage = covered
        .iter()
        .any(|c| c.x == 0 && c.y == 0 && c.width >= frame_width && c.height >= frame_height);
    if full_coverage {
        return Vec::new();
    }

    let mut result: Vec<DamageRegion> = Vec::new();
    for r in regions {
        // Worklist of uncovered pieces of `r`.
        let mut pieces = vec![*r];
        for c in covered {
            let mut next_pieces = Vec::new();
            for p in pieces {
                // Intersection of p and c (empty when disjoint).
                let ix = p.x.max(c.x);
                let iy = p.y.max(c.y);
                let ix2 = (p.x + p.width).min(c.x + c.width);
                let iy2 = (p.y + p.height).min(c.y + c.height);
                if ix >= ix2 || iy >= iy2 {
                    // Disjoint: p survives untouched.
                    next_pieces.push(p);
                    continue;
                }
                // Clip p against the intersection, emitting the 4 side bands.
                // Left band.
                if ix > p.x {
                    next_pieces.push(DamageRegion::new(p.x, p.y, ix - p.x, p.height));
                }
                // Right band.
                let p_x2 = p.x + p.width;
                if ix2 < p_x2 {
                    next_pieces.push(DamageRegion::new(ix2, p.y, p_x2 - ix2, p.height));
                }
                // Top band (between left/right clip).
                if iy > p.y {
                    next_pieces.push(DamageRegion::new(ix, p.y, ix2 - ix, iy - p.y));
                }
                // Bottom band (between left/right clip).
                let p_y2 = p.y + p.height;
                if iy2 < p_y2 {
                    next_pieces.push(DamageRegion::new(ix, iy2, ix2 - ix, p_y2 - iy2));
                }
            }
            pieces = next_pieces;
            if pieces.is_empty() {
                break;
            }
            if pieces.len() > MAX_PIECES {
                // Fragmentation cap: return the region un-subtracted rather
                // than let the worklist multiply further. Oversending is
                // always safe here (probe-union semantics); under-sending
                // would leave stale pixels.
                pieces = vec![*r];
                break;
            }
        }
        result.extend(pieces);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulator_absorb_replaces_instead_of_extending() {
        // Regression: the inline code extended the debt with a send set that
        // already contained the debt, doubling it on every consecutive skip.
        let mut acc = DamageAccumulator::new();
        let a = DamageRegion::new(0, 0, 100, 100);
        acc.absorb(vec![a]); // frame 1 skipped: debt = {A}
        // Frame 2: send set = debt {A} + fresh {B}
        let b = DamageRegion::new(200, 200, 50, 50);
        acc.absorb(vec![a, b]); // must REPLACE, not extend → {A, B}
        let taken = acc.take();
        assert_eq!(taken.len(), 2, "no doubling: {taken:?}");
        assert!(acc.is_empty());
    }

    #[test]
    fn accumulator_survives_long_skip_streak_linearly() {
        // 10 consecutive skips (an Interactive sub-threshold streak) must not
        // grow the debt exponentially.
        let mut acc = DamageAccumulator::new();
        let fresh = DamageRegion::new(10, 10, 40, 40);
        let mut send_set = vec![fresh];
        for _ in 0..10 {
            acc.absorb(send_set.clone());
            // next frame's send set = debt + fresh
            let mut next = acc.take();
            next.push(fresh);
            send_set = next;
        }
        acc.absorb(send_set);
        assert!(
            acc.len() <= 2,
            "streak of 10 skips must stay bounded, got {}",
            acc.len()
        );
    }

    #[test]
    fn accumulator_merges_overlapping_debt() {
        let mut acc = DamageAccumulator::new();
        // Two identical regions (e.g. re-detected at consecutive probes).
        acc.absorb(vec![
            DamageRegion::new(0, 0, 100, 100),
            DamageRegion::new(0, 0, 100, 100),
        ]);
        assert_eq!(acc.len(), 1, "overlaps must merge: {:?}", acc.regions);
        // Ratio over merged debt cannot double-count area.
        let ratio = compute_damage_ratio(&acc.regions, 200, 200);
        assert!((ratio - 0.25).abs() < 1e-6, "ratio {ratio}");
    }

    #[test]
    fn accumulator_cap_collapses_to_bounding_union() {
        let mut acc = DamageAccumulator::new();
        // Push well past the cap with mutually non-adjacent regions.
        let mut set = Vec::new();
        for i in 0..(DamageAccumulator::DEFAULT_CAP + 64) {
            let x = u32::try_from(i % 64).unwrap() * 1000;
            let y = u32::try_from(i / 64).unwrap() * 1000;
            set.push(DamageRegion::new(x, y, 10, 10));
        }
        acc.absorb(set);
        assert!(
            acc.len() <= DamageAccumulator::DEFAULT_CAP,
            "cap must bind, got {}",
            acc.len()
        );
    }

    #[test]
    fn subtract_caps_fragmentation_instead_of_exploding() {
        // Pathological covered set: many thin strips crossing the region
        // would multiply pieces toward 4^|covered| without the cap.
        let region = DamageRegion::new(0, 0, 4096, 4096);
        let covered: Vec<DamageRegion> = (0..200)
            .map(|i| {
                let y = i * 20;
                // Horizontal strip crossing the full width, with a gap so
                // full_coverage doesn't trigger.
                DamageRegion::new(0, y, 4000, 10)
            })
            .collect();
        let out = subtract_regions(&[region], &covered, 4096, 4096);
        // Either the exact subtraction finished cheaply or the cap fell back
        // to the whole region — both bounded results are acceptable; the
        // exponential blowup (4^200) is not.
        assert!(
            out.len() <= 256 || (out.len() == 1 && out[0] == region),
            "bounded output expected, got {}",
            out.len()
        );
    }

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
        assert!(compute_damage_ratio(&[], 1920, 1080).abs() < 1e-6);
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
        assert!(compute_damage_ratio(&regions, 0, 0).abs() < 1e-6);
    }

    #[test]
    fn avc444_forced_off_by_config() {
        let (enabled, reason) = resolve_avc444_enabled("avc420", true, true);
        assert!(!enabled);
        assert!(reason.contains("AVC420 forced"));
    }

    #[test]
    fn avc444_requested_and_supported() {
        assert!(resolve_avc444_enabled("avc444", true, true).0);
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
        assert!(resolve_avc444_enabled("auto", true, true).0);
        assert!(!resolve_avc444_enabled("auto", false, true).0);
        assert!(!resolve_avc444_enabled("auto", true, false).0);
    }

    #[test]
    fn avc444_unrecognized_pref_behaves_as_auto() {
        // An unknown codec string falls through to the auto branch.
        assert!(resolve_avc444_enabled("nonsense", true, true).0);
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
        assert!(eval.drop_rate.abs() < 1e-6);
    }

    #[test]
    fn trust_below_threshold_never_distrusts() {
        let eval = evaluate_compositor_trust(5.0, 0, 15.0, 3);
        assert_eq!(eval.new_consecutive_count, 0);
        assert!(!eval.should_distrust);
    }

    #[test]
    fn trust_single_high_sample_does_not_distrust() {
        // One sample over threshold is below required_consecutive=3.
        let eval = evaluate_compositor_trust(90.0, 0, 15.0, 3);
        assert_eq!(eval.new_consecutive_count, 1);
        assert!(!eval.should_distrust);
    }

    #[test]
    fn trust_consecutive_samples_trigger_distrust() {
        let mut count = 0;
        for _ in 0..2 {
            let eval = evaluate_compositor_trust(90.0, count, 15.0, 3);
            count = eval.new_consecutive_count;
            assert!(!eval.should_distrust);
        }
        let eval = evaluate_compositor_trust(90.0, count, 15.0, 3);
        assert_eq!(eval.new_consecutive_count, 3);
        assert!(eval.should_distrust);
    }

    #[test]
    fn trust_interrupting_low_sample_resets_counter() {
        let first = evaluate_compositor_trust(90.0, 0, 15.0, 3);
        assert_eq!(first.new_consecutive_count, 1);
        let second = evaluate_compositor_trust(90.0, first.new_consecutive_count, 15.0, 3);
        assert_eq!(second.new_consecutive_count, 2);
        // An in-range sample interrupts the streak — counter resets, not decrements.
        let reset = evaluate_compositor_trust(2.0, second.new_consecutive_count, 15.0, 3);
        assert_eq!(reset.new_consecutive_count, 0);
        assert!(!reset.should_distrust);
    }

    #[test]
    fn trust_negative_divergence_uses_absolute_value() {
        // Compositor under-reporting damage is just as untrustworthy as
        // over-reporting; divergence is compared by magnitude.
        let eval = evaluate_compositor_trust(-90.0, 0, 15.0, 3);
        assert_eq!(eval.new_consecutive_count, 1);
    }

    // === subtract_regions (probe-union support) ===

    /// Total area of a region list, for asserting coverage conservation.
    fn total_area(regions: &[DamageRegion]) -> u64 {
        regions.iter().map(DamageRegion::area).sum()
    }

    #[test]
    fn subtract_disjoint_regions_returned_unchanged() {
        let regions = [DamageRegion::new(0, 0, 100, 100)];
        let covered = [DamageRegion::new(500, 500, 100, 100)];
        let out = subtract_regions(&regions, &covered, 1920, 1080);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], regions[0]);
    }

    #[test]
    fn subtract_fully_covered_region_erased() {
        let regions = [DamageRegion::new(100, 100, 100, 100)];
        let covered = [DamageRegion::new(0, 0, 1920, 1080)]; // whole frame
        let out = subtract_regions(&regions, &covered, 1920, 1080);
        assert!(out.is_empty());
    }

    #[test]
    fn subtract_partial_coverage_conserves_area() {
        // Region 200x200 at (0,0); covered is the left half 100x200.
        // Remainder must be exactly the right half: area 20000.
        let regions = [DamageRegion::new(0, 0, 200, 200)];
        let covered = [DamageRegion::new(0, 0, 100, 200)];
        let out = subtract_regions(&regions, &covered, 1920, 1080);
        assert_eq!(total_area(&out), 20_000);
        // Every remainder rect must start at x=100.
        for r in &out {
            assert_eq!(r.x, 100);
            assert_eq!(r.width, 100);
        }
    }

    #[test]
    fn subtract_center_hole_produces_four_bands() {
        // A covered rectangle in the middle of a region leaves 4 side bands
        // whose total area is region − intersection.
        let regions = [DamageRegion::new(0, 0, 300, 300)];
        let covered = [DamageRegion::new(100, 100, 100, 100)];
        let out = subtract_regions(&regions, &covered, 1920, 1080);
        // 90000 − 10000 = 80000
        assert_eq!(total_area(&out), 80_000);
        // 4 bands: top, bottom, left, right
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn subtract_multiple_covered_regions() {
        // Two covered strips slicing a region into three columns.
        let regions = [DamageRegion::new(0, 0, 300, 100)];
        let covered = [
            DamageRegion::new(100, 0, 10, 100),
            DamageRegion::new(200, 0, 10, 100),
        ];
        let out = subtract_regions(&regions, &covered, 1920, 1080);
        // 30000 − 2000 = 28000
        assert_eq!(total_area(&out), 28_000);
    }

    #[test]
    fn subtract_empty_inputs() {
        assert!(subtract_regions(&[], &[DamageRegion::new(0, 0, 10, 10)], 100, 100).is_empty());
        let regions = [DamageRegion::new(0, 0, 10, 10)];
        let out = subtract_regions(&regions, &[], 100, 100);
        assert_eq!(out.len(), 1);
    }
}
