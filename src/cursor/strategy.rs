//! Cursor rendering strategies
//!
//! This module defines different strategies for cursor handling,
//! each optimized for different scenarios.

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::predictor::{CursorPredictor, PredictorConfig};

/// Cursor rendering mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CursorMode {
    /// Client-side cursor rendering (lowest latency)
    /// Server sends cursor shape and position metadata.
    #[default]
    Metadata,

    /// Cursor painted into video frames
    /// Works with all clients but has video latency.
    Painted,

    /// Hidden cursor (for touch/pen input)
    Hidden,

    /// Predictive cursor rendering (Premium)
    /// Uses physics-based prediction to compensate for latency.
    Predictive,
}

impl CursorMode {
    /// Get human-readable description
    pub fn description(&self) -> &'static str {
        match self {
            Self::Metadata => "Client-side rendering (lowest latency)",
            Self::Painted => "Painted in video (maximum compatibility)",
            Self::Hidden => "Cursor hidden",
            Self::Predictive => "Predictive rendering (compensates for latency)",
        }
    }

    /// Check if this mode requires server-side cursor compositing
    ///
    /// Predictive rendering is client-side, same as Metadata; it only changes
    /// which position gets sent, not who draws the cursor.
    pub fn requires_compositing(&self) -> bool {
        matches!(self, Self::Painted)
    }
}

impl std::fmt::Display for CursorMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Metadata => write!(f, "Metadata"),
            Self::Painted => write!(f, "Painted"),
            Self::Hidden => write!(f, "Hidden"),
            Self::Predictive => write!(f, "Predictive"),
        }
    }
}

impl std::str::FromStr for CursorMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "metadata" | "client" | "default" => Ok(Self::Metadata),
            "painted" | "embedded" | "composite" => Ok(Self::Painted),
            "hidden" | "none" | "off" => Ok(Self::Hidden),
            "predictive" | "predict" | "physics" => Ok(Self::Predictive),
            _ => Err(format!("Unknown cursor mode: {s}")),
        }
    }
}

/// Configuration for cursor strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CursorStrategyConfig {
    /// Cursor rendering mode
    #[serde(default)]
    pub mode: CursorMode,

    /// Enable automatic mode selection based on latency
    #[serde(default = "default_true")]
    pub auto_mode: bool,

    /// Latency threshold (ms) above which to enable predictive mode
    #[serde(default = "default_latency_threshold")]
    pub predictive_latency_threshold_ms: u32,

    /// Predictor configuration (for predictive mode)
    #[serde(default)]
    pub predictor: PredictorConfig,

    /// Cursor update rate for separate stream (FPS)
    #[serde(default = "default_cursor_fps")]
    pub cursor_update_fps: u32,
}

fn default_true() -> bool {
    true
}
fn default_latency_threshold() -> u32 {
    100
}
fn default_cursor_fps() -> u32 {
    60
}

impl Default for CursorStrategyConfig {
    fn default() -> Self {
        Self {
            mode: CursorMode::Metadata,
            auto_mode: true,
            predictive_latency_threshold_ms: 100,
            predictor: PredictorConfig::default(),
            cursor_update_fps: 60,
        }
    }
}

impl From<&crate::config::types::CursorPredictorConfig> for PredictorConfig {
    fn from(cfg: &crate::config::types::CursorPredictorConfig) -> Self {
        Self {
            history_size: cfg.history_size,
            lookahead_ms: cfg.lookahead_ms,
            velocity_smoothing: cfg.velocity_smoothing,
            acceleration_smoothing: cfg.acceleration_smoothing,
            max_prediction_distance: cfg.max_prediction_distance,
            min_velocity_threshold: cfg.min_velocity_threshold,
            stop_convergence_rate: cfg.stop_convergence_rate,
        }
    }
}

impl From<&crate::config::types::CursorConfig> for CursorStrategyConfig {
    fn from(cfg: &crate::config::types::CursorConfig) -> Self {
        let mode = cfg.mode.parse().unwrap_or_else(|e| {
            warn!(
                "Invalid cursor.mode {:?} in config, falling back to Metadata: {e}",
                cfg.mode
            );
            CursorMode::Metadata
        });
        Self {
            mode,
            auto_mode: cfg.auto_mode,
            predictive_latency_threshold_ms: cfg.predictive_latency_threshold_ms,
            predictor: (&cfg.predictor).into(),
            cursor_update_fps: cfg.cursor_update_fps,
        }
    }
}

/// Cursor strategy manager
///
/// Manages cursor rendering mode and handles automatic
/// mode switching based on measured latency.
pub struct CursorStrategy {
    /// Configuration
    config: CursorStrategyConfig,

    /// Current active mode
    active_mode: CursorMode,

    /// Cursor predictor (for predictive mode)
    predictor: Option<CursorPredictor>,

    /// Measured network latency (ms)
    measured_latency_ms: u32,

    /// Current cursor position
    current_position: (i32, i32),

    /// Current cursor shape (for metadata mode)
    current_shape: Option<CursorShape>,

    /// Round-robin cache of recently-sent shapes, keyed by `CursorMeta::id`.
    shape_cache: CursorShapeCache,

    /// Whether a `HidePointer` update has already been sent for the current
    /// hidden/no-cursor span, so it's sent exactly once per transition.
    hidden_sent: bool,

    /// Whether the active mode was pinned by an explicit `set_mode` call.
    /// When set, `auto_select_mode` must not clobber it: latency samples
    /// arrive continuously (the RTT prediction loop), and reverting an
    /// explicitly chosen mode to `config.mode`/Predictive on every sample
    /// would silently undo it. Measured live on the Parrot VM: config
    /// `mode = "painted"` + `auto_mode = true` + threshold 0 flipped the
    /// mode to Predictive on the FIRST latency sample, so the Painted-mode
    /// HidePointer was never sent (9981c8c shipped exactly that config).
    mode_pinned: bool,

    /// Consecutive frames with no `SPA_META_Cursor` attached. Drives the
    /// runtime Painted auto-selection in `observe_metadata_cursors`.
    metadata_absent_frames: u32,

    /// One-way latch: this capture path has delivered cursor metadata at
    /// least once. Once set, the runtime Painted flip never engages — a
    /// path that CAN deliver metadata keeps client-side rendering even if
    /// some frames are absent (the pointer can be off-output).
    metadata_ever_seen: bool,
}

/// Consecutive metadata-absent frames after which the runtime Painted
/// auto-selection engages (only when the configured mode is the Metadata
/// default and no explicit `set_mode` pin exists). The old cursor-theme
/// workaround's `PENDING_ABSENT_FRAMES = 3` is the precedent; a slightly
/// larger N costs ~2 frames (~35 ms at 60 fps) of visible double cursor at
/// connect and avoids misclassifying a first frame where the pointer is
/// briefly off-output.
const METADATA_ABSENT_FRAMES_LIMIT: u32 = 5;

/// Number of pointer-cache slots this crate assumes it can safely use.
///
/// Chosen without visibility into the client's actual negotiated
/// `pointerCacheSize` (MS-RDPBCGR 2.2.7.1.5): that value is read and
/// enforced internally by `ironrdp-server` (dropping New Pointer Update/
/// CachedPointer emission entirely when it's zero) but isn't exposed back
/// to this crate. A small, conservative slot count is safe regardless of
/// what a real client negotiates — MS-RDPBCGR only ever requires that value
/// be nonzero to enable the cache at all — at the cost of evicting and
/// re-encoding sooner than a larger cache would for a session that cycles
/// through many distinct shapes.
const SHAPE_CACHE_CAPACITY: usize = 8;

/// Round-robin cache of recently-sent cursor shapes, keyed by the
/// compositor's `CursorMeta::id`.
#[derive(Debug)]
struct CursorShapeCache {
    slots: Vec<Option<u32>>,
    next_evict: usize,
}

impl CursorShapeCache {
    fn new(capacity: usize) -> Self {
        Self {
            slots: vec![None; capacity.max(1)],
            next_evict: 0,
        }
    }

    /// Non-mutating lookup: `Some(index)` if `id` is currently cached.
    fn lookup(&self, id: u32) -> Option<u16> {
        self.slots
            .iter()
            .position(|slot| *slot == Some(id))
            .map(|idx| idx as u16)
    }

    /// Claim a cache slot for `id`, evicting the oldest entry if full.
    /// Callers must only call this once they're actually about to send the
    /// full shape at the returned index — inserting speculatively (e.g.
    /// before confirming a shape was successfully encoded) would leave the
    /// cache claiming a slot the client was never actually sent, so a later
    /// `Hit` on that id would reference nothing real.
    fn insert(&mut self, id: u32) -> u16 {
        let idx = self.next_evict;
        self.slots[idx] = Some(id);
        self.next_evict = (self.next_evict + 1) % self.slots.len();
        idx as u16
    }
}

/// Cursor shape information
#[derive(Debug, Clone)]
pub struct CursorShape {
    /// Width in pixels
    pub width: u32,
    /// Height in pixels
    pub height: u32,
    /// Hotspot X offset
    pub hotspot_x: u32,
    /// Hotspot Y offset
    pub hotspot_y: u32,
    /// Pixel data (RGBA)
    pub data: Vec<u8>,
}

impl CursorStrategy {
    pub fn new(config: CursorStrategyConfig) -> Self {
        let predictor = if config.mode == CursorMode::Predictive {
            Some(CursorPredictor::new(config.predictor.clone()))
        } else {
            None
        };

        Self {
            active_mode: config.mode,
            predictor,
            measured_latency_ms: 0,
            current_position: (0, 0),
            current_shape: None,
            shape_cache: CursorShapeCache::new(SHAPE_CACHE_CAPACITY),
            hidden_sent: false,
            mode_pinned: false,
            metadata_absent_frames: 0,
            metadata_ever_seen: false,
            config,
        }
    }

    /// Look up a compositor cursor id (`CursorMeta::id`) in the shape cache.
    /// `Some(index)` means the caller should send
    /// `DisplayUpdate::CachedPointer(index)` instead of re-encoding;
    /// `None` means the caller must encode the shape and then call
    /// `cache_shape` (only once it actually has something to send).
    ///
    /// Callers must not pass `id == 0` (the compositor's "invalid/no
    /// cursor" sentinel) — that's a hide signal, not a shape to cache.
    pub fn lookup_shape_cache(&self, id: u32) -> Option<u16> {
        debug_assert_ne!(id, 0, "id == 0 is a hide signal, not a cacheable shape");
        self.shape_cache.lookup(id)
    }

    /// Claim a cache slot for `id` and return its index. Call this only
    /// once a shape has actually been encoded and is about to be sent —
    /// see `CursorShapeCache::insert`'s doc for why claiming speculatively
    /// would corrupt the cache.
    pub fn cache_shape(&mut self, id: u32) -> u16 {
        debug_assert_ne!(id, 0, "id == 0 is a hide signal, not a cacheable shape");
        self.shape_cache.insert(id)
    }

    /// Whether a `HidePointer` update still needs to be sent for the
    /// current hidden/no-cursor span. Marks it sent so callers only send it
    /// once; `note_visible` clears the flag on the next real shape/position.
    pub fn needs_hide_update(&mut self) -> bool {
        if self.hidden_sent {
            false
        } else {
            self.hidden_sent = true;
            true
        }
    }

    /// Clear the hidden/no-cursor tracking after sending a real update.
    pub fn note_visible(&mut self) {
        self.hidden_sent = false;
    }

    /// Observe whether this frame's capture metadata carried a cursor, and
    /// resolve the active mode from the evidence.
    ///
    /// On paths that never deliver `SPA_META_Cursor` (measured: KWin 6.3.6
    /// zkde virtual outputs; portal_generic's direct channel structurally),
    /// the compositor still paints the cursor into the video — a Metadata
    /// (or Predictive, which is metadata-driven) session then sends no
    /// pointer PDUs at all and the client draws its own arrow on top of
    /// the painted one: the double cursor. After
    /// `METADATA_ABSENT_FRAMES_LIMIT` consecutive absent frames, a session
    /// whose *configured* mode is Metadata or Predictive (no explicit
    /// Painted/Hidden operator choice, no `set_mode` pin) flips to Painted
    /// so the transparent-shape PDU takes pointer ownership.
    ///
    /// The flip is deliberately UNPINNED: `auto_select_mode`'s semantic
    /// guard already protects Painted, and staying unpinned lets a metadata
    /// stream that starts later (Portal/Mutter session, fixed KWin) flip
    /// the mode back to Metadata for client-side rendering. An explicit
    /// `set_mode` pin always wins over both directions, and once metadata
    /// has ever been seen the Painted flip never engages.
    pub fn observe_metadata_cursors(&mut self, present: bool) {
        if present {
            self.metadata_ever_seen = true;
            self.metadata_absent_frames = 0;
            if !self.mode_pinned && self.active_mode == CursorMode::Painted {
                info!(
                    "cursor metadata delivered — switching Painted -> Metadata \
                     (client-side rendering)"
                );
                self.apply_mode(CursorMode::Metadata);
            }
            return;
        }
        if self.metadata_ever_seen || self.mode_pinned {
            self.metadata_absent_frames = 0;
            return;
        }
        // saturating_add: on a metadata-less path this counter otherwise
        // grows unboundedly (u32 overflow after ~828 days at 60 fps — a
        // debug build would panic).
        self.metadata_absent_frames = self.metadata_absent_frames.saturating_add(1);
        // The `active_mode != Painted` guard makes the flip (and its log)
        // fire exactly once per transition: on a metadata-less path every
        // frame past the limit would otherwise re-enter this block and
        // bury the journal 30-60 times per second.
        // Predictive is treated like Metadata here: prediction is driven
        // by cursor-metadata samples, so a Predictive-configured session
        // on a metadata-less path sends no pointer PDUs either — the same
        // double cursor — and equally benefits from the Painted flip.
        if self.metadata_absent_frames >= METADATA_ABSENT_FRAMES_LIMIT
            && matches!(
                self.config.mode,
                CursorMode::Metadata | CursorMode::Predictive
            )
            && self.active_mode != CursorMode::Painted
        {
            info!(
                "no cursor metadata after {} frames — auto-selecting Painted mode \
                 (compositor-painted cursor + transparent client pointer shape)",
                self.metadata_absent_frames
            );
            self.apply_mode(CursorMode::Painted);
        }
    }

    /// Update cursor position
    pub fn update_position(&mut self, x: i32, y: i32) {
        self.current_position = (x, y);

        if let Some(ref mut predictor) = self.predictor {
            predictor.update(x, y);
        }
    }

    /// Update cursor shape
    pub fn update_shape(&mut self, shape: CursorShape) {
        self.current_shape = Some(shape);
    }

    /// Update measured network latency
    pub fn update_latency(&mut self, latency_ms: u32) {
        self.measured_latency_ms = latency_ms;

        // Auto-switch mode if enabled
        if self.config.auto_mode {
            self.auto_select_mode();
        }

        // Update predictor lookahead based on latency
        if let Some(ref mut predictor) = self.predictor {
            // Use 50-100% of measured latency as lookahead
            let lookahead = (latency_ms as f32 * 0.75).clamp(20.0, 150.0);
            predictor.set_lookahead(lookahead);
        }
    }

    /// Get cursor position to render
    ///
    /// Returns predicted position if in predictive mode,
    /// otherwise returns actual position.
    pub fn render_position(&mut self) -> (i32, i32) {
        match self.active_mode {
            CursorMode::Predictive => {
                if let Some(ref mut predictor) = self.predictor {
                    predictor.get_predicted_position()
                } else {
                    self.current_position
                }
            }
            _ => self.current_position,
        }
    }

    /// Get actual cursor position
    pub fn actual_position(&self) -> (i32, i32) {
        self.current_position
    }

    /// Get current cursor shape
    pub fn shape(&self) -> Option<&CursorShape> {
        self.current_shape.as_ref()
    }

    /// Get active cursor mode
    pub fn mode(&self) -> CursorMode {
        self.active_mode
    }

    /// Switch the active mode (predictor lifecycle included). Shared by the
    /// explicit `set_mode` and the internal `auto_select_mode`; does not
    /// touch the pin.
    fn apply_mode(&mut self, mode: CursorMode) {
        if mode != self.active_mode {
            debug!("Cursor mode changed: {:?} -> {:?}", self.active_mode, mode);
            self.active_mode = mode;

            // Create or destroy predictor as needed
            match mode {
                CursorMode::Predictive => {
                    if self.predictor.is_none() {
                        self.predictor = Some(CursorPredictor::new(self.config.predictor.clone()));
                    }
                }
                _ => {
                    self.predictor = None;
                }
            }
        }
    }

    /// Set cursor mode explicitly. Explicit modes are pinned against
    /// `auto_select_mode` — see `mode_pinned` for why an unpinned explicit
    /// mode gets clobbered by the first latency sample.
    pub fn set_mode(&mut self, mode: CursorMode) {
        self.apply_mode(mode);
        self.mode_pinned = true;
    }

    /// Re-arm per-connection cursor state. `CursorStrategy` outlives RDP
    /// connections (the display handler owns it for the process lifetime),
    /// so `hidden_sent` from a previous client would suppress the
    /// HidePointer the new connection needs. Call from the client-
    /// activation path. (The active mode itself is NOT reset here — it is
    /// resolved once from config at construction; per-connection mode
    /// selection is future work.)
    pub fn rearm_for_connection(&mut self) {
        self.hidden_sent = false;
    }

    /// Get measured latency
    pub fn latency(&self) -> u32 {
        self.measured_latency_ms
    }

    /// Configured cursor update rate (Hz) for the periodic prediction
    /// re-emission loop.
    pub fn update_fps(&self) -> u32 {
        self.config.cursor_update_fps
    }

    /// Check if cursor compositing is needed
    pub fn needs_compositing(&self) -> bool {
        self.active_mode.requires_compositing()
    }

    /// Get cursor predictor (if in predictive mode)
    pub fn predictor(&self) -> Option<&CursorPredictor> {
        self.predictor.as_ref()
    }

    fn auto_select_mode(&mut self) {
        // An explicitly set mode wins, and prediction is meaningless when
        // the cursor is painted into the video or suppressed entirely.
        // BOTH guards are needed: config-driven modes (the production path)
        // never pass through `set_mode`, so the pin alone leaves them
        // unprotected — measured live: config mode="painted" + auto_mode
        // flipped to Predictive once RTT crossed the threshold, so the
        // Painted-mode HidePointer was never sent.
        if self.mode_pinned || matches!(self.active_mode, CursorMode::Painted | CursorMode::Hidden)
        {
            return;
        }

        let should_predict = self.measured_latency_ms > self.config.predictive_latency_threshold_ms;

        let new_mode = if should_predict {
            CursorMode::Predictive
        } else {
            self.config.mode // Fall back to configured default
        };

        if new_mode != self.active_mode {
            debug!(
                "Auto-switching cursor mode: {:?} -> {:?} (latency={}ms)",
                self.active_mode, new_mode, self.measured_latency_ms
            );
            self.apply_mode(new_mode);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cursor_mode_from_str() {
        assert_eq!(
            "metadata".parse::<CursorMode>().unwrap(),
            CursorMode::Metadata
        );
        assert_eq!(
            "predictive".parse::<CursorMode>().unwrap(),
            CursorMode::Predictive
        );
        assert_eq!("hidden".parse::<CursorMode>().unwrap(), CursorMode::Hidden);
    }

    #[test]
    fn test_default_config() {
        let config = CursorStrategyConfig::default();
        assert_eq!(config.mode, CursorMode::Metadata);
        assert!(config.auto_mode);
        assert_eq!(config.predictive_latency_threshold_ms, 100);
    }

    #[test]
    fn test_auto_mode_switching() {
        let mut config = CursorStrategyConfig::default();
        config.auto_mode = true;
        config.predictive_latency_threshold_ms = 100;

        let mut strategy = CursorStrategy::new(config);

        // Low latency - should stay in metadata mode
        strategy.update_latency(50);
        assert_eq!(strategy.mode(), CursorMode::Metadata);

        // High latency - should switch to predictive
        strategy.update_latency(150);
        assert_eq!(strategy.mode(), CursorMode::Predictive);

        // Low latency again - should switch back
        strategy.update_latency(50);
        assert_eq!(strategy.mode(), CursorMode::Metadata);
    }

    #[test]
    fn test_explicit_mode_pins_against_auto_select() {
        // Regression (measured live on the Parrot VM): a config of
        // mode="painted" + auto_mode + threshold 0 flipped the mode to
        // Predictive on the FIRST latency sample, so Painted's HidePointer
        // was never sent (shipped 9981c8c). An explicit set_mode must win.
        let mut config = CursorStrategyConfig::default();
        config.auto_mode = true;
        config.predictive_latency_threshold_ms = 100;

        let mut strategy = CursorStrategy::new(config);
        strategy.set_mode(CursorMode::Painted);

        // Latency samples arrive continuously (RTT loop); none may clobber.
        strategy.update_latency(500);
        assert_eq!(strategy.mode(), CursorMode::Painted);
        strategy.update_latency(5);
        assert_eq!(strategy.mode(), CursorMode::Painted);
    }

    #[test]
    fn test_config_driven_painted_mode_survives_auto_select() {
        // The production path: config mode="painted" reaches the strategy
        // via `new` (never `set_mode`), so the pin alone does not protect
        // it — the semantic guard in auto_select_mode must.
        let mut config = CursorStrategyConfig::default();
        config.mode = CursorMode::Painted;
        config.auto_mode = true;
        config.predictive_latency_threshold_ms = 100;

        let mut strategy = CursorStrategy::new(config);
        assert_eq!(strategy.mode(), CursorMode::Painted);

        strategy.update_latency(500);
        assert_eq!(strategy.mode(), CursorMode::Painted);
        strategy.update_latency(5);
        assert_eq!(strategy.mode(), CursorMode::Painted);
    }

    #[test]
    fn test_config_driven_hidden_mode_survives_auto_select() {
        let mut config = CursorStrategyConfig::default();
        config.mode = CursorMode::Hidden;
        config.auto_mode = true;
        config.predictive_latency_threshold_ms = 100;

        let mut strategy = CursorStrategy::new(config);
        strategy.update_latency(500);
        assert_eq!(strategy.mode(), CursorMode::Hidden);
    }

    #[test]
    fn test_runtime_painted_auto_select_when_metadata_absent() {
        // Fresh install, default config (mode=metadata): a capture path
        // that never delivers cursor metadata must flip to Painted so the
        // transparent shape PDU takes pointer ownership (the double-cursor
        // fix, out of the box).
        let mut strategy = CursorStrategy::new(CursorStrategyConfig::default());
        assert_eq!(strategy.mode(), CursorMode::Metadata);

        for _ in 0..(METADATA_ABSENT_FRAMES_LIMIT - 1) {
            strategy.observe_metadata_cursors(false);
            assert_eq!(strategy.mode(), CursorMode::Metadata);
        }
        strategy.observe_metadata_cursors(false);
        assert_eq!(strategy.mode(), CursorMode::Painted);

        // Late metadata flips it back (client-side rendering), and the
        // ever-seen latch keeps it Metadata even across later absences.
        strategy.observe_metadata_cursors(true);
        assert_eq!(strategy.mode(), CursorMode::Metadata);
        for _ in 0..(METADATA_ABSENT_FRAMES_LIMIT + 3) {
            strategy.observe_metadata_cursors(false);
        }
        assert_eq!(strategy.mode(), CursorMode::Metadata);
    }

    #[test]
    fn test_explicit_config_mode_wins_over_runtime_auto_select() {
        // An operator's explicit non-default choice must not be overridden
        // by the runtime resolver in EITHER direction.
        let mut config = CursorStrategyConfig::default();
        config.mode = CursorMode::Hidden;
        let mut strategy = CursorStrategy::new(config);
        for _ in 0..(METADATA_ABSENT_FRAMES_LIMIT + 5) {
            strategy.observe_metadata_cursors(false);
        }
        assert_eq!(strategy.mode(), CursorMode::Hidden);

        strategy.observe_metadata_cursors(true);
        assert_eq!(strategy.mode(), CursorMode::Hidden);
    }

    #[test]
    fn test_set_mode_pin_wins_over_runtime_auto_select() {
        let mut strategy = CursorStrategy::new(CursorStrategyConfig::default());
        strategy.set_mode(CursorMode::Predictive);
        for _ in 0..(METADATA_ABSENT_FRAMES_LIMIT + 5) {
            strategy.observe_metadata_cursors(false);
        }
        strategy.observe_metadata_cursors(true);
        assert_eq!(strategy.mode(), CursorMode::Predictive);
    }

    #[test]
    fn test_predictive_config_also_flips_to_painted_when_metadata_absent() {
        // Predictive is metadata-driven; without metadata it sends no
        // pointer PDUs either — the same double cursor as Metadata.
        let mut config = CursorStrategyConfig::default();
        config.mode = CursorMode::Predictive;
        let mut strategy = CursorStrategy::new(config);
        assert_eq!(strategy.mode(), CursorMode::Predictive);

        for _ in 0..METADATA_ABSENT_FRAMES_LIMIT {
            strategy.observe_metadata_cursors(false);
        }
        assert_eq!(strategy.mode(), CursorMode::Painted);
    }

    #[test]
    fn test_runtime_auto_select_is_transition_guarded() {
        // On a metadata-less path the counter keeps growing past the limit
        // (saturating at u32::MAX far beyond this test's reach); the mode
        // must stay Painted and the transition must fire once, not per
        // frame. This is a transition-stability test, not a counter-
        // saturation test — 1,105 iterations exercise the guard, not
        // integer overflow.
        let mut strategy = CursorStrategy::new(CursorStrategyConfig::default());
        for _ in 0..(METADATA_ABSENT_FRAMES_LIMIT + 100) {
            strategy.observe_metadata_cursors(false);
        }
        assert_eq!(strategy.mode(), CursorMode::Painted);
        // Still stable after many more frames: no re-flip churn, no
        // per-frame work beyond the counter tick.
        for _ in 0..1000 {
            strategy.observe_metadata_cursors(false);
        }
        assert_eq!(strategy.mode(), CursorMode::Painted);
    }

    #[test]
    fn test_predictive_mode_creates_predictor() {
        let mut config = CursorStrategyConfig::default();
        config.mode = CursorMode::Predictive;

        let strategy = CursorStrategy::new(config);
        assert!(strategy.predictor().is_some());
    }

    #[test]
    fn test_compositing_required() {
        assert!(!CursorMode::Metadata.requires_compositing());
        assert!(CursorMode::Painted.requires_compositing());
        assert!(!CursorMode::Predictive.requires_compositing());
        assert!(!CursorMode::Hidden.requires_compositing());
    }

    #[test]
    fn test_lookup_shape_cache_unseen_id_is_none() {
        let strategy = CursorStrategy::new(CursorStrategyConfig::default());
        assert_eq!(strategy.lookup_shape_cache(5), None);
    }

    #[test]
    fn test_cache_shape_then_lookup_is_a_hit_at_the_same_index() {
        let mut strategy = CursorStrategy::new(CursorStrategyConfig::default());
        let index = strategy.cache_shape(5);
        assert_eq!(strategy.lookup_shape_cache(5), Some(index));
    }

    #[test]
    fn test_lookup_without_cache_shape_never_reports_a_hit() {
        // A lookup alone must never claim a slot: only cache_shape does.
        let strategy = CursorStrategy::new(CursorStrategyConfig::default());
        assert_eq!(strategy.lookup_shape_cache(5), None);
        assert_eq!(strategy.lookup_shape_cache(5), None);
    }

    #[test]
    fn test_cache_shape_distinguishes_different_ids() {
        let mut strategy = CursorStrategy::new(CursorStrategyConfig::default());
        let idx_a = strategy.cache_shape(5);
        let idx_b = strategy.cache_shape(6);
        assert_ne!(idx_a, idx_b);
        assert_eq!(strategy.lookup_shape_cache(5), Some(idx_a));
        assert_eq!(strategy.lookup_shape_cache(6), Some(idx_b));
    }

    #[test]
    fn test_cache_shape_survives_interleaved_ids() {
        // Cycling between two shapes must hit the cache for both, not just
        // the immediately-previous one.
        let mut strategy = CursorStrategy::new(CursorStrategyConfig::default());
        let idx_a = strategy.cache_shape(1);
        let idx_b = strategy.cache_shape(2);
        assert_eq!(strategy.lookup_shape_cache(1), Some(idx_a));
        assert_eq!(strategy.lookup_shape_cache(2), Some(idx_b));
        assert_eq!(strategy.lookup_shape_cache(1), Some(idx_a));
    }

    #[test]
    fn test_cache_shape_evicts_oldest_slot_when_full() {
        let mut strategy = CursorStrategy::new(CursorStrategyConfig::default());
        for id in 1..=SHAPE_CACHE_CAPACITY as u32 {
            strategy.cache_shape(id);
        }
        // Cache is now full (ids 1..=CAPACITY, one per slot). One more
        // distinct id must evict the oldest (id 1) rather than reuse a
        // still-live slot.
        let evicted_slot = strategy.cache_shape(SHAPE_CACHE_CAPACITY as u32 + 1);
        assert_eq!(evicted_slot, 0);
        // id 1's original slot (0) has been reused: id 1 is no longer cached.
        assert_eq!(strategy.lookup_shape_cache(1), None);
    }

    #[test]
    fn test_needs_hide_update_fires_once_per_span() {
        let mut strategy = CursorStrategy::new(CursorStrategyConfig::default());
        assert!(strategy.needs_hide_update());
        assert!(!strategy.needs_hide_update());
        strategy.note_visible();
        assert!(strategy.needs_hide_update());
    }

    #[test]
    fn test_config_conversion_parses_mode_and_carries_predictor_fields() {
        let config = crate::config::types::CursorConfig {
            mode: "predictive".to_string(),
            auto_mode: false,
            predictive_latency_threshold_ms: 42,
            cursor_update_fps: 30,
            predictor: crate::config::types::CursorPredictorConfig {
                history_size: 3,
                lookahead_ms: 10.0,
                velocity_smoothing: 0.1,
                acceleration_smoothing: 0.2,
                max_prediction_distance: 50,
                min_velocity_threshold: 1.0,
                stop_convergence_rate: 0.5,
            },
        };

        let strategy_config: CursorStrategyConfig = (&config).into();
        assert_eq!(strategy_config.mode, CursorMode::Predictive);
        assert!(!strategy_config.auto_mode);
        assert_eq!(strategy_config.predictive_latency_threshold_ms, 42);
        assert_eq!(strategy_config.cursor_update_fps, 30);
        assert_eq!(strategy_config.predictor.history_size, 3);
        assert_eq!(strategy_config.predictor.max_prediction_distance, 50);
    }

    #[test]
    fn test_config_conversion_falls_back_to_metadata_on_invalid_mode() {
        let mut config = crate::config::types::CursorConfig::default();
        config.mode = "not-a-real-mode".to_string();

        let strategy_config: CursorStrategyConfig = (&config).into();
        assert_eq!(strategy_config.mode, CursorMode::Metadata);
    }
}
