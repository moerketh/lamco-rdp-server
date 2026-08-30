//! RDP Input Handler Implementation
//!
//! Implements the IronRDP `RdpServerInputHandler` trait to forward input events
//! from RDP clients to the Wayland compositor via Portal RemoteDesktop API.
//!
//! # Overview
//!
//! This module bridges the synchronous IronRDP input event callbacks with the
//! asynchronous Portal API, providing complete keyboard and mouse input forwarding
//! with full scancode translation, modifier tracking, and coordinate transformation.
//!
//! # Architecture
//!
//! ```text
//! RDP Client                    WrdInputHandler                 Wayland
//! ━━━━━━━━━━                    ━━━━━━━━━━━━━━━                 ━━━━━━━
//!
//! Keyboard Event ─────────────> KeyboardEvent
//!   scancode=0x1E                     │
//!   pressed=true                      ├─> KeyboardHandler
//!                                     │     └─> Scancode translation
//!                                     │         (0x1E → evdev KEY_A)
//!                                     │
//!                                     ├─> Portal API
//!                                     │     └─> notify_keyboard_keycode()
//!                                     │
//!                                     └─────────────────────────> Input Stack
//!                                                                   └─> Key Press
//!
//! Mouse Event ────────────────> MouseEvent::Move
//!   x=960, y=540                     │
//!                                    ├─> CoordinateTransformer
//!                                    │     └─> RDP coords → Wayland coords
//!                                    │
//!                                    ├─> Portal API
//!                                    │     └─> notify_pointer_motion_absolute()
//!                                    │
//!                                    └─────────────────────────> Input Stack
//!                                                                  └─> Mouse Move
//! ```
//!
//! # Async/Sync Bridging
//!
//! IronRDP's `RdpServerInputHandler` trait has synchronous methods (`fn`, not `async fn`),
//! but Portal API calls are asynchronous. We bridge this gap by:
//!
//! 1. Trait method called synchronously by IronRDP
//! 2. Clone Arc references to shared state
//! 3. Spawn `tokio::spawn()` async task
//! 4. Task performs async Portal API calls
//! 5. Fire-and-forget (RDP doesn't expect acknowledgment for input events)
//!
//! This pattern ensures the synchronous trait method returns immediately while
//! Portal operations proceed asynchronously without blocking.
//!
//! # Example
//!
//! ```ignore
//! use lamco_rdp_server::server::WrdInputHandler;
//! use lamco_rdp_server::portal::RemoteDesktopManager;
//! use lamco_rdp_server::input::MonitorInfo;
//! use std::sync::Arc;
//!
//! let portal = Arc::new(RemoteDesktopManager::new(/* ... */).await?);
//! let session = portal.create_session().await?;
//! let monitors = vec![/* MonitorInfo instances */];
//!
//! let handler = WrdInputHandler::new(portal, session, monitors)?;
//!
//! // Handler is now ready to receive input events from IronRDP
//! // Events are automatically forwarded to Wayland via Portal
//! ```

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use ironrdp_server::{
    KeyboardEvent as IronKeyboardEvent, MouseEvent as IronMouseEvent, RdpServerInputHandler,
};
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, error, info, trace, warn};

use crate::input::{
    CoordinateTransformer, InputError, KeyboardHandler, MonitorInfo, MouseButton, MouseHandler,
};

/// Map a Unicode code point to an evdev keycode and whether Shift is needed.
/// Covers printable ASCII (0x20-0x7E) on US QWERTY layout.
fn unicode_to_evdev(cp: u16) -> Option<(u32, bool)> {
    // evdev keycodes from lamco-rdp-input::mapper::keycodes
    const KEY_SPACE: u32 = 57;
    const KEY_1: u32 = 2;
    const KEY_2: u32 = 3;
    const KEY_3: u32 = 4;
    const KEY_4: u32 = 5;
    const KEY_5: u32 = 6;
    const KEY_6: u32 = 7;
    const KEY_7: u32 = 8;
    const KEY_8: u32 = 9;
    const KEY_9: u32 = 10;
    const KEY_0: u32 = 11;
    const KEY_MINUS: u32 = 12;
    const KEY_EQUAL: u32 = 13;
    const KEY_TAB: u32 = 15;
    const KEY_Q: u32 = 16;
    const KEY_W: u32 = 17;
    const KEY_E: u32 = 18;
    const KEY_R: u32 = 19;
    const KEY_T: u32 = 20;
    const KEY_Y: u32 = 21;
    const KEY_U: u32 = 22;
    const KEY_I: u32 = 23;
    const KEY_O: u32 = 24;
    const KEY_P: u32 = 25;
    const KEY_LEFTBRACE: u32 = 26;
    const KEY_RIGHTBRACE: u32 = 27;
    const KEY_ENTER: u32 = 28;
    const KEY_A: u32 = 30;
    const KEY_S: u32 = 31;
    const KEY_D: u32 = 32;
    const KEY_F: u32 = 33;
    const KEY_G: u32 = 34;
    const KEY_H: u32 = 35;
    const KEY_J: u32 = 36;
    const KEY_K: u32 = 37;
    const KEY_L: u32 = 38;
    const KEY_SEMICOLON: u32 = 39;
    const KEY_APOSTROPHE: u32 = 40;
    const KEY_GRAVE: u32 = 41;
    const KEY_BACKSLASH: u32 = 43;
    const KEY_Z: u32 = 44;
    const KEY_X: u32 = 45;
    const KEY_C: u32 = 46;
    const KEY_V: u32 = 47;
    const KEY_B: u32 = 48;
    const KEY_N: u32 = 49;
    const KEY_M: u32 = 50;
    const KEY_COMMA: u32 = 51;
    const KEY_DOT: u32 = 52;
    const KEY_SLASH: u32 = 53;

    // (evdev_keycode, needs_shift)
    match cp {
        // Whitespace
        0x20 => Some((KEY_SPACE, false)),        // ' '
        0x09 => Some((KEY_TAB, false)),          // Tab
        0x0A | 0x0D => Some((KEY_ENTER, false)), // Newline / CR

        // Digits
        0x30 => Some((KEY_0, false)), // '0'
        0x31 => Some((KEY_1, false)), // '1'
        0x32 => Some((KEY_2, false)), // '2'
        0x33 => Some((KEY_3, false)), // '3'
        0x34 => Some((KEY_4, false)), // '4'
        0x35 => Some((KEY_5, false)), // '5'
        0x36 => Some((KEY_6, false)), // '6'
        0x37 => Some((KEY_7, false)), // '7'
        0x38 => Some((KEY_8, false)), // '8'
        0x39 => Some((KEY_9, false)), // '9'

        // Lowercase letters
        0x61 => Some((KEY_A, false)), // 'a'
        0x62 => Some((KEY_B, false)),
        0x63 => Some((KEY_C, false)),
        0x64 => Some((KEY_D, false)),
        0x65 => Some((KEY_E, false)),
        0x66 => Some((KEY_F, false)),
        0x67 => Some((KEY_G, false)),
        0x68 => Some((KEY_H, false)),
        0x69 => Some((KEY_I, false)),
        0x6A => Some((KEY_J, false)),
        0x6B => Some((KEY_K, false)),
        0x6C => Some((KEY_L, false)),
        0x6D => Some((KEY_M, false)),
        0x6E => Some((KEY_N, false)),
        0x6F => Some((KEY_O, false)),
        0x70 => Some((KEY_P, false)),
        0x71 => Some((KEY_Q, false)),
        0x72 => Some((KEY_R, false)),
        0x73 => Some((KEY_S, false)),
        0x74 => Some((KEY_T, false)),
        0x75 => Some((KEY_U, false)),
        0x76 => Some((KEY_V, false)),
        0x77 => Some((KEY_W, false)),
        0x78 => Some((KEY_X, false)),
        0x79 => Some((KEY_Y, false)),
        0x7A => Some((KEY_Z, false)), // 'z'

        // Uppercase letters (same keys, with Shift)
        0x41 => Some((KEY_A, true)), // 'A'
        0x42 => Some((KEY_B, true)),
        0x43 => Some((KEY_C, true)),
        0x44 => Some((KEY_D, true)),
        0x45 => Some((KEY_E, true)),
        0x46 => Some((KEY_F, true)),
        0x47 => Some((KEY_G, true)),
        0x48 => Some((KEY_H, true)),
        0x49 => Some((KEY_I, true)),
        0x4A => Some((KEY_J, true)),
        0x4B => Some((KEY_K, true)),
        0x4C => Some((KEY_L, true)),
        0x4D => Some((KEY_M, true)),
        0x4E => Some((KEY_N, true)),
        0x4F => Some((KEY_O, true)),
        0x50 => Some((KEY_P, true)),
        0x51 => Some((KEY_Q, true)),
        0x52 => Some((KEY_R, true)),
        0x53 => Some((KEY_S, true)),
        0x54 => Some((KEY_T, true)),
        0x55 => Some((KEY_U, true)),
        0x56 => Some((KEY_V, true)),
        0x57 => Some((KEY_W, true)),
        0x58 => Some((KEY_X, true)),
        0x59 => Some((KEY_Y, true)),
        0x5A => Some((KEY_Z, true)), // 'Z'

        // Symbols (unshifted)
        0x2D => Some((KEY_MINUS, false)),      // '-'
        0x3D => Some((KEY_EQUAL, false)),      // '='
        0x5B => Some((KEY_LEFTBRACE, false)),  // '['
        0x5D => Some((KEY_RIGHTBRACE, false)), // ']'
        0x5C => Some((KEY_BACKSLASH, false)),  // '\'
        0x3B => Some((KEY_SEMICOLON, false)),  // ';'
        0x27 => Some((KEY_APOSTROPHE, false)), // '\''
        0x60 => Some((KEY_GRAVE, false)),      // '`'
        0x2C => Some((KEY_COMMA, false)),      // ','
        0x2E => Some((KEY_DOT, false)),        // '.'
        0x2F => Some((KEY_SLASH, false)),      // '/'

        // Symbols (shifted)
        0x21 => Some((KEY_1, true)),          // '!'
        0x40 => Some((KEY_2, true)),          // '@'
        0x23 => Some((KEY_3, true)),          // '#'
        0x24 => Some((KEY_4, true)),          // '$'
        0x25 => Some((KEY_5, true)),          // '%'
        0x5E => Some((KEY_6, true)),          // '^'
        0x26 => Some((KEY_7, true)),          // '&'
        0x2A => Some((KEY_8, true)),          // '*'
        0x28 => Some((KEY_9, true)),          // '('
        0x29 => Some((KEY_0, true)),          // ')'
        0x5F => Some((KEY_MINUS, true)),      // '_'
        0x2B => Some((KEY_EQUAL, true)),      // '+'
        0x7B => Some((KEY_LEFTBRACE, true)),  // '{'
        0x7D => Some((KEY_RIGHTBRACE, true)), // '}'
        0x7C => Some((KEY_BACKSLASH, true)),  // '|'
        0x3A => Some((KEY_SEMICOLON, true)),  // ':'
        0x22 => Some((KEY_APOSTROPHE, true)), // '"'
        0x7E => Some((KEY_GRAVE, true)),      // '~'
        0x3C => Some((KEY_COMMA, true)),      // '<'
        0x3E => Some((KEY_DOT, true)),        // '>'
        0x3F => Some((KEY_SLASH, true)),      // '?'

        _ => None,
    }
}

fn input_injection_err(e: impl std::fmt::Display) -> InputError {
    InputError::PortalError(format!("Input injection error: {e}"))
}

/// Lamco RDP Input Handler
///
/// Bridges IronRDP input events to our Portal-based input injection system.
/// This handler receives keyboard and mouse events from RDP clients and forwards
/// them to the Wayland compositor through the RemoteDesktop portal.
///
/// Since IronRDP's trait methods are synchronous but portal operations are async,
/// we use channels and spawned tasks to bridge the gap.
/// Input event for batching/multiplexing
#[derive(Debug)]
pub enum InputEvent {
    /// Keyboard event from RDP client
    Keyboard(IronKeyboardEvent),
    /// Mouse event from RDP client
    Mouse(IronMouseEvent),
}

/// Coalesce consecutive Move and RelMove events in a mouse-event batch.
///
/// Remote-desktop clients (notably mstsc) stream hundreds of mouse-move
/// events per millisecond during window manipulation. Each event becomes a
/// separate `wl_pointer.motion` request that must be flushed to the
/// compositor. Under software-rendering load the compositor cannot drain
/// the Wayland socket fast enough; flushes return EAGAIN/WouldBlock and the
/// input subsystem fails.
///
/// Coalescing rules:
/// - Consecutive `Move { x, y }` events collapse to a single Move with the
///   latest position. Intermediate trail positions are visually equivalent
///   to the final position from the compositor's perspective.
/// - Consecutive `RelMove { x, y }` events sum into a single RelMove with
///   the total delta. Equivalent end-effect.
/// - When a Move/RelMove run is interrupted by the other variant, the
///   pending run flushes before the new run starts.
/// - Non-move events (buttons, scroll, etc.) pass through unchanged, but
///   flush any pending Move/RelMove first so the button/scroll occurs at
///   the final cursor position rather than at an arbitrary intermediate
///   point.
fn coalesce_mouse_batch(events: Vec<IronMouseEvent>) -> Vec<IronMouseEvent> {
    if events.len() < 2 {
        return events;
    }
    fn flush_pending(
        out: &mut Vec<IronMouseEvent>,
        abs: &mut Option<(u16, u16)>,
        rel: &mut Option<(i32, i32)>,
    ) {
        if let Some((x, y)) = abs.take() {
            out.push(IronMouseEvent::Move { x, y });
        }
        if let Some((dx, dy)) = rel.take() {
            out.push(IronMouseEvent::RelMove { x: dx, y: dy });
        }
    }

    let mut out: Vec<IronMouseEvent> = Vec::with_capacity(events.len());
    let mut pending_abs: Option<(u16, u16)> = None;
    let mut pending_rel: Option<(i32, i32)> = None;

    for ev in events {
        match ev {
            IronMouseEvent::Move { x, y } => {
                if pending_rel.is_some() {
                    flush_pending(&mut out, &mut pending_abs, &mut pending_rel);
                }
                pending_abs = Some((x, y));
            }
            IronMouseEvent::RelMove { x, y } => {
                if pending_abs.is_some() {
                    flush_pending(&mut out, &mut pending_abs, &mut pending_rel);
                }
                let (dx, dy) = pending_rel.unwrap_or((0, 0));
                pending_rel = Some((dx.saturating_add(x), dy.saturating_add(y)));
            }
            other => {
                flush_pending(&mut out, &mut pending_abs, &mut pending_rel);
                out.push(other);
            }
        }
    }
    flush_pending(&mut out, &mut pending_abs, &mut pending_rel);
    out
}

/// Lamco RDP input handler that bridges IronRDP input events to Portal injection
///
/// Receives keyboard and mouse events from RDP clients and injects them
/// into the Wayland compositor via the Portal RemoteDesktop API.
pub struct LamcoInputHandler {
    /// Session handle for input injection (abstraction over Portal/Mutter)
    session_handle: Arc<dyn crate::session::SessionHandle>,

    /// Keyboard event handler (pub for multiplexer access)
    pub keyboard_handler: Arc<Mutex<KeyboardHandler>>,

    /// Mouse event handler (pub for multiplexer access)
    pub mouse_handler: Arc<Mutex<MouseHandler>>,

    /// Coordinate transformer for multi-monitor support (pub for multiplexer access)
    pub coordinate_transformer: Arc<Mutex<CoordinateTransformer>>,

    /// Primary stream node ID for input injection (PipeWire node ID)
    primary_stream_id: u32,

    /// Input event queue sender (for multiplexer - bounded with drop policy)
    input_tx: mpsc::Sender<InputEvent>,
    /// Capture->desktop scale factors (resolution support).
    ///
    /// RDP pointer coordinates arrive in DESKTOP space; input injection
    /// targets the compositor in CAPTURE space. When the client's chosen
    /// desktop differs from the capture (hyperv_drm fixed modes), events are
    /// inverse-mapped here. Shared with the display handler so the factors
    /// track live resizes.
    scale_factors: Arc<parking_lot::RwLock<crate::server::frame_scaler::ScaleFactors>>,
}

impl LamcoInputHandler {
    pub fn new(
        session_handle: Arc<dyn crate::session::SessionHandle>,
        monitors: Vec<MonitorInfo>,
        primary_stream_id: u32,
        input_tx: mpsc::Sender<InputEvent>,
        mut input_rx: mpsc::Receiver<InputEvent>,
        mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
        scale_factors: Arc<parking_lot::RwLock<crate::server::frame_scaler::ScaleFactors>>,
    ) -> Result<Self, InputError> {
        let keyboard_handler = Arc::new(Mutex::new(KeyboardHandler::new()));
        let mouse_handler = Arc::new(Mutex::new(MouseHandler::new()));

        let coordinate_transformer = Arc::new(Mutex::new(CoordinateTransformer::new(monitors)?));

        debug!(
            "Input handler using PipeWire stream node ID: {}",
            primary_stream_id
        );

        // Start input batching task (10ms windows for responsive typing)
        // Receives from multiplexer input queue, batches, and sends to Portal
        let session_handle_clone = Arc::clone(&session_handle);
        let keyboard_clone = Arc::clone(&keyboard_handler);
        let mouse_clone = Arc::clone(&mouse_handler);
        let coord_clone = Arc::clone(&coordinate_transformer);
        let scale_clone = Arc::clone(&scale_factors);

        tokio::spawn(async move {
            let mut keyboard_batch = Vec::with_capacity(16);
            let mut mouse_batch = Vec::with_capacity(16);
            let mut last_flush = Instant::now();
            let batch_interval = tokio::time::Duration::from_millis(10);

            // Rate-limit input injection errors to avoid log spam when the
            // portal session becomes unresponsive (e.g. PipeWire stream pauses)
            let consecutive_mouse_errors = AtomicU64::new(0);
            let consecutive_kbd_errors = AtomicU64::new(0);

            loop {
                tokio::select! {
                    Some(event) = input_rx.recv() => {
                        match event {
                            InputEvent::Keyboard(kbd) => {
                                trace!("📥 Input queue: received keyboard event");
                                keyboard_batch.push(kbd);
                            }
                            InputEvent::Mouse(mouse) => {
                                trace!("📥 Input queue: received mouse event");
                                mouse_batch.push(mouse);
                            }
                        }
                    }

                    () = tokio::time::sleep_until(tokio::time::Instant::from_std(last_flush + batch_interval)) => {
                        // Process keyboard batch
                        if !keyboard_batch.is_empty() {
                            trace!("🔄 Input batching: flushing {} keyboard events", keyboard_batch.len());
                        }
                        for kbd_event in keyboard_batch.drain(..) {
                            if let Err(e) = Self::handle_keyboard_event_impl(
                                &session_handle_clone,
                                &keyboard_clone,
                                kbd_event
                            ).await {
                                let count = consecutive_kbd_errors.fetch_add(1, Ordering::Relaxed) + 1;
                                if count == 1 {
                                    warn!("Keyboard injection failed: {e}");
                                } else if count.is_power_of_two() {
                                    warn!("Keyboard injection failed ({count} consecutive): {e}");
                                }
                            } else {
                                let prev = consecutive_kbd_errors.swap(0, Ordering::Relaxed);
                                if prev > 1 {
                                    info!("Portal keyboard injection recovered after {prev} failures");
                                }
                            }
                        }

                        // Process mouse batch — coalesce consecutive Move/RelMove
                        // first to limit Wayland flush count under mstsc-style
                        // bursts (see coalesce_mouse_batch for rules).
                        let coalesced: Vec<IronMouseEvent> =
                            coalesce_mouse_batch(std::mem::take(&mut mouse_batch));
                        if !coalesced.is_empty() {
                            trace!(
                                "🔄 Input batching: flushing {} mouse events (after coalesce)",
                                coalesced.len()
                            );
                        }
                        for mouse_event in coalesced {
                            if let Err(e) = Self::handle_mouse_event_impl(
                                &session_handle_clone,
                                &mouse_clone,
                                &coord_clone,
                                &scale_clone,
                                mouse_event,
                                primary_stream_id
                            ).await {
                                let count = consecutive_mouse_errors.fetch_add(1, Ordering::Relaxed) + 1;
                                if count == 1 {
                                    warn!("Mouse injection failed: {e}");
                                } else if count.is_power_of_two() {
                                    warn!("Mouse injection failed ({count} consecutive): {e}");
                                }
                            } else {
                                let prev = consecutive_mouse_errors.swap(0, Ordering::Relaxed);
                                if prev > 1 {
                                    info!("Portal mouse injection recovered after {prev} failures");
                                }
                            }
                        }

                        last_flush = Instant::now();
                    }

                    _ = shutdown_rx.recv() => {
                        info!("🛑 Input batching task received shutdown signal");
                        break;
                    }
                }
            }

            let mouse_errs = consecutive_mouse_errors.load(Ordering::Relaxed);
            let kbd_errs = consecutive_kbd_errors.load(Ordering::Relaxed);
            if mouse_errs > 0 || kbd_errs > 0 {
                info!(
                    "Input batching task stopped (pending errors: mouse={mouse_errs}, kbd={kbd_errs})"
                );
            } else {
                info!("Input batching task stopped");
            }
        });

        info!("Input batching task started (REAL task, 10ms flush interval)");

        Ok(Self {
            session_handle,
            keyboard_handler,
            mouse_handler,
            coordinate_transformer,
            primary_stream_id,
            input_tx,
            scale_factors,
        })
    }

    /// Activate the input subsystem (deferred EIS creation).
    ///
    /// Called when the first RDP client connects. Delegates to the
    /// session handle's `activate_input()` which creates the EIS
    /// connection on-demand.
    pub async fn activate_input(&self) -> anyhow::Result<()> {
        self.session_handle.activate_input().await
    }

    /// Notify input handler that client reconnected
    ///
    /// Resets internal state to handle new client connection.
    /// Call this when reconnection is detected (e.g., display_updates channel recreated).
    pub async fn notify_reconnection(&self) {
        info!("🔄 Input handler: Client reconnected, resetting state");

        {
            let mut kbd = self.keyboard_handler.lock().await;
            *kbd = KeyboardHandler::new();
            debug!("Keyboard handler state reset");
        }

        {
            let mut mouse = self.mouse_handler.lock().await;
            *mouse = MouseHandler::new();
            debug!("Mouse handler state reset");
        }

        info!("✅ Input handler ready for reconnected client");
    }

    /// Update coordinate transformer when monitor configuration changes
    ///
    /// This should be called when the RDP client requests a different resolution
    /// or when monitor configuration changes.
    pub async fn update_monitors(&self, monitors: Vec<MonitorInfo>) -> Result<(), InputError> {
        let mut transformer = self.coordinate_transformer.lock().await;
        *transformer = CoordinateTransformer::new(monitors)?;
        debug!("Updated monitor configuration");
        Ok(())
    }

    /// Handle keyboard event implementation (static for batching task)
    async fn handle_keyboard_event_impl(
        session_handle: &Arc<dyn crate::session::SessionHandle>,
        keyboard_handler: &Arc<Mutex<KeyboardHandler>>,
        event: IronKeyboardEvent,
    ) -> Result<(), InputError> {
        let mut keyboard = keyboard_handler.lock().await;

        match event {
            IronKeyboardEvent::Pressed { code, extended } => {
                // Log V key specifically to trace Ctrl+V paste operations
                if code == 0x2F {
                    // V key scancode
                    info!(
                        "⌨️ V key pressed (scancode=0x{:02X}, extended={})",
                        code, extended
                    );
                }
                trace!("Keyboard pressed: code={}, extended={}", code, extended);

                let kbd_event = keyboard.handle_key_down(code as u16, extended, false)?;

                let keycode = match kbd_event {
                    crate::input::KeyboardEvent::KeyDown { keycode, .. }
                    | crate::input::KeyboardEvent::KeyRepeat { keycode, .. } => keycode,
                    crate::input::KeyboardEvent::KeyUp { keycode, .. } => {
                        // handle_key_down returned KeyUp (shouldn't happen but handle gracefully)
                        warn!(
                            "handle_key_down returned KeyUp for code {} - using keycode anyway",
                            code
                        );
                        keycode
                    }
                    #[expect(
                        unreachable_patterns,
                        reason = "defensive: future KeyboardEvent variants"
                    )]
                    other => {
                        error!("handle_key_down returned unexpected event: {:?}", other);
                        return Err(InputError::InvalidKeyEvent(format!(
                            "Unexpected event type: {other:?}"
                        )));
                    }
                };

                // Log V key injection to Portal
                if keycode == 47 {
                    // evdev KEY_V
                    info!(
                        "⌨️ Injecting V key press to Portal (evdev keycode={})",
                        keycode
                    );
                }

                session_handle
                    .notify_keyboard_keycode(keycode as i32, true)
                    .await
                    .map_err(input_injection_err)?;
            }

            IronKeyboardEvent::Released { code, extended } => {
                // Log V key releases
                if code == 0x2F {
                    // V key scancode
                    info!(
                        "⌨️ V key released (scancode=0x{:02X}, extended={})",
                        code, extended
                    );
                }
                trace!("Keyboard released: code={}, extended={}", code, extended);

                let kbd_event = keyboard.handle_key_up(code as u16, extended, false)?;

                let keycode = match kbd_event {
                    crate::input::KeyboardEvent::KeyUp { keycode, .. } => keycode,
                    _ => {
                        return Err(InputError::InvalidKeyEvent(
                            "Unexpected event type".to_string(),
                        ));
                    }
                };

                // Log V key injection release to Portal
                if keycode == 47 {
                    // evdev KEY_V
                    info!(
                        "⌨️ Injecting V key release to Portal (evdev keycode={})",
                        keycode
                    );
                }

                session_handle
                    .notify_keyboard_keycode(keycode as i32, false)
                    .await
                    .map_err(input_injection_err)?;
            }

            IronKeyboardEvent::UnicodePressed(unicode) => {
                if let Some((keycode, needs_shift)) = unicode_to_evdev(unicode) {
                    // Fast path: ASCII characters mapped to evdev keycodes
                    debug!(
                        "Unicode press 0x{:04X} -> evdev {} (shift={})",
                        unicode, keycode, needs_shift
                    );
                    // KEY_LEFTSHIFT = 42
                    if needs_shift {
                        session_handle
                            .notify_keyboard_keycode(42, true)
                            .await
                            .map_err(input_injection_err)?;
                    }
                    session_handle
                        .notify_keyboard_keycode(keycode as i32, true)
                        .await
                        .map_err(input_injection_err)?;
                } else {
                    // Keysym path: CJK, accented, and other non-ASCII characters.
                    // XKB Unicode keysyms: 0x01000000 + Unicode code point.
                    let keysym = 0x0100_0000_u32 + u32::from(unicode);
                    debug!("Unicode press 0x{:04X} -> keysym 0x{:08X}", unicode, keysym);
                    session_handle
                        .notify_keyboard_keysym(keysym, true)
                        .await
                        .map_err(input_injection_err)?;
                }
            }

            IronKeyboardEvent::UnicodeReleased(unicode) => {
                if let Some((keycode, needs_shift)) = unicode_to_evdev(unicode) {
                    debug!(
                        "Unicode release 0x{:04X} -> evdev {} (shift={})",
                        unicode, keycode, needs_shift
                    );
                    session_handle
                        .notify_keyboard_keycode(keycode as i32, false)
                        .await
                        .map_err(input_injection_err)?;
                    if needs_shift {
                        session_handle
                            .notify_keyboard_keycode(42, false)
                            .await
                            .map_err(input_injection_err)?;
                    }
                } else {
                    let keysym = 0x0100_0000_u32 + u32::from(unicode);
                    debug!(
                        "Unicode release 0x{:04X} -> keysym 0x{:08X}",
                        unicode, keysym
                    );
                    session_handle
                        .notify_keyboard_keysym(keysym, false)
                        .await
                        .map_err(input_injection_err)?;
                }
            }

            IronKeyboardEvent::Synchronize(flags) => {
                trace!("Keyboard synchronize: {:?}", flags);
                // Update toggle key states based on sync flags
                // The flags tell us the client's current Caps/Num/Scroll lock states
                // We should sync our local state but portal doesn't have direct sync API
                // This is handled implicitly when keys are pressed
            }
        }

        Ok(())
    }

    /// Handle mouse event with full error handling and logging
    /// Handle mouse event implementation (static for batching task)
    async fn handle_mouse_event_impl(
        session_handle: &Arc<dyn crate::session::SessionHandle>,
        mouse_handler: &Arc<Mutex<MouseHandler>>,
        coordinate_transformer: &Arc<Mutex<CoordinateTransformer>>,
        scale_factors: &Arc<parking_lot::RwLock<crate::server::frame_scaler::ScaleFactors>>,
        event: IronMouseEvent,
        stream_id: u32,
    ) -> Result<(), InputError> {
        let mut mouse = mouse_handler.lock().await;
        let mut transformer = coordinate_transformer.lock().await;

        // Resolution support: RDP pointer coordinates arrive in DESKTOP
        // space. Inverse-map them into CAPTURE (compositor) space before the
        // monitor transformer sees them, so final injection matches what the
        // client is pointing at on its (possibly scaled) desktop. Identity
        // fast path is exact — no arithmetic when factors are (1,1).
        let inv = {
            let f = scale_factors.read();
            (f.x, f.y)
        };
        let map_event = |x: u16, y: u16| -> (u16, u16) {
            if inv == ((1, 1), (1, 1)) {
                return (x, y);
            }
            // desktop -> capture: multiply by den/num per axis (the inverse
            // of the video direction's num/den), clamped to pointer range.
            let fx = ((x as u64 * inv.0.1 as u64) / inv.0.0.max(1) as u64).min(u16::MAX as u64);
            let fy = ((y as u64 * inv.1.1 as u64) / inv.1.0.max(1) as u64).min(u16::MAX as u64);
            (fx as u16, fy as u16)
        };

        match event {
            IronMouseEvent::Move { x, y } => {
                trace!("Mouse move: x={}, y={}", x, y);

                let (x, y) = map_event(x, y);
                let mouse_event =
                    mouse.handle_absolute_move(x as u32, y as u32, &mut transformer)?;

                let (stream_x, stream_y) = match mouse_event {
                    crate::input::MouseEvent::Move { x, y, .. } => (x, y),
                    _ => {
                        return Err(InputError::InvalidMouseEvent(
                            "Unexpected event type".to_string(),
                        ));
                    }
                };

                session_handle
                    .notify_pointer_motion_absolute(stream_id, stream_x, stream_y)
                    .await
                    .map_err(input_injection_err)?;
            }

            IronMouseEvent::RelMove { x, y } => {
                trace!("Mouse relative move: dx={}, dy={}", x, y);

                let mouse_event = mouse.handle_relative_move(x, y, &mut transformer)?;

                let (stream_x, stream_y) = match mouse_event {
                    crate::input::MouseEvent::Move { x, y, .. } => (x, y),
                    _ => {
                        return Err(InputError::InvalidMouseEvent(
                            "Unexpected event type".to_string(),
                        ));
                    }
                };

                // We converted relative to absolute already
                session_handle
                    .notify_pointer_motion_absolute(stream_id, stream_x, stream_y)
                    .await
                    .map_err(input_injection_err)?;
            }

            // The rebased fork collapsed the ten per-button press/release
            // variants into one `Button { button, pressed }` arm (upstream
            // reshaped MouseEvent around a shared MouseButton identity).
            IronMouseEvent::Button {
                button, pressed, ..
            } => {
                use ironrdp_server::MouseButton as IronMouseButton;

                // evdev BTN_* codes: 272 LEFT, 273 RIGHT, 274 MIDDLE,
                // 275 SIDE (back), 276 EXTRA (forward).
                let (lamco_button, evdev_code) = match button {
                    IronMouseButton::Left => (MouseButton::Left, 272),
                    IronMouseButton::Right => (MouseButton::Right, 273),
                    IronMouseButton::Middle => (MouseButton::Middle, 274),
                    IronMouseButton::X1 => (MouseButton::Extra1, 275),
                    IronMouseButton::X2 => (MouseButton::Extra2, 276),
                    _ => {
                        trace!(?button, "Ignoring unknown mouse button");
                        return Ok(());
                    }
                };

                if pressed {
                    mouse.handle_button_down(lamco_button)?;
                } else {
                    mouse.handle_button_up(lamco_button)?;
                }
                session_handle
                    .notify_pointer_button(evdev_code, pressed)
                    .await
                    .map_err(input_injection_err)?;
            }

            // Relative-motion button events (MouseRelPdu path): apply the
            // delta first so button state and injected position agree.
            IronMouseEvent::ButtonRel {
                x,
                y,
                button,
                pressed,
            } => {
                use ironrdp_server::MouseButton as IronMouseButton;

                let mouse_event = mouse.handle_relative_move(x, y, &mut transformer)?;
                if let crate::input::MouseEvent::Move {
                    x: stream_x,
                    y: stream_y,
                    ..
                } = mouse_event
                {
                    session_handle
                        .notify_pointer_motion_absolute(stream_id, stream_x, stream_y)
                        .await
                        .map_err(input_injection_err)?;
                }

                let (lamco_button, evdev_code) = match button {
                    IronMouseButton::Left => (MouseButton::Left, 272),
                    IronMouseButton::Right => (MouseButton::Right, 273),
                    IronMouseButton::Middle => (MouseButton::Middle, 274),
                    IronMouseButton::X1 => (MouseButton::Extra1, 275),
                    IronMouseButton::X2 => (MouseButton::Extra2, 276),
                    _ => {
                        trace!(?button, "Ignoring unknown relative mouse button");
                        return Ok(());
                    }
                };

                if pressed {
                    mouse.handle_button_down(lamco_button)?;
                } else {
                    mouse.handle_button_up(lamco_button)?;
                }
                session_handle
                    .notify_pointer_button(evdev_code, pressed)
                    .await
                    .map_err(input_injection_err)?;
            }

            IronMouseEvent::VerticalScroll { value } => {
                // RDP scroll units are in 120ths
                mouse.handle_scroll(0, value as i32)?;
                let delta_y = (value as f64 / 120.0) * 15.0;
                session_handle
                    .notify_pointer_axis(0.0, delta_y)
                    .await
                    .map_err(input_injection_err)?;
            }

            IronMouseEvent::Scroll { x, y } => {
                mouse.handle_scroll(x, y)?;
                let delta_x = (x as f64 / 120.0) * 15.0;
                let delta_y = (y as f64 / 120.0) * 15.0;
                session_handle
                    .notify_pointer_axis(delta_x, delta_y)
                    .await
                    .map_err(input_injection_err)?;
            }

            // MouseEvent is #[non_exhaustive]; upstream may add variants
            // (e.g. new pointer PDUs). Unknown events are logged and dropped
            // rather than failing the input pipeline.
            unexpected => {
                trace!(?unexpected, "Unhandled mouse event variant");
            }
        }

        Ok(())
    }
}

impl RdpServerInputHandler for LamcoInputHandler {
    fn keyboard(&mut self, event: IronKeyboardEvent) {
        trace!("⌨️  Input multiplexer: routing keyboard to queue");
        if let Err(e) = self.input_tx.try_send(InputEvent::Keyboard(event)) {
            error!("Failed to queue keyboard event for batching: {}", e);
        }
    }

    fn mouse(&mut self, event: IronMouseEvent) {
        trace!("🖱️  Input multiplexer: routing mouse to queue");
        // Position events (Move/RelMove) are coalesce-safe: if the channel
        // is full the dropped event will be effectively replaced by the
        // next arriving position. Buttons and scrolls cannot be dropped
        // without semantic loss, so they remain ERROR.
        let is_position_event = matches!(
            &event,
            IronMouseEvent::Move { .. } | IronMouseEvent::RelMove { .. }
        );
        if let Err(e) = self.input_tx.try_send(InputEvent::Mouse(event)) {
            if is_position_event {
                trace!("Dropped queued mouse position event: {}", e);
            } else {
                error!("Failed to queue mouse event for batching: {}", e);
            }
        }
    }
}

/// RdpServer needs ownership but we want to share state
impl Clone for LamcoInputHandler {
    fn clone(&self) -> Self {
        Self {
            session_handle: Arc::clone(&self.session_handle),
            keyboard_handler: Arc::clone(&self.keyboard_handler),
            mouse_handler: Arc::clone(&self.mouse_handler),
            coordinate_transformer: Arc::clone(&self.coordinate_transformer),
            primary_stream_id: self.primary_stream_id,
            input_tx: self.input_tx.clone(),
            scale_factors: Arc::clone(&self.scale_factors),
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_input_handler_clone() {
        // Verify clone compiles and works
        // Full tests require portal mocking
    }

    #[test]
    fn test_unicode_to_evdev_ascii() {
        use super::unicode_to_evdev;

        // Space
        assert_eq!(unicode_to_evdev(0x20), Some((57, false)));
        // Lowercase 'a'
        assert_eq!(unicode_to_evdev(0x61), Some((30, false)));
        // Uppercase 'A' (shift)
        assert_eq!(unicode_to_evdev(0x41), Some((30, true)));
        // Digit '0'
        assert_eq!(unicode_to_evdev(0x30), Some((11, false)));
        // Exclamation '!' (shift+1)
        assert_eq!(unicode_to_evdev(0x21), Some((2, true)));
        // Tab
        assert_eq!(unicode_to_evdev(0x09), Some((15, false)));
        // Enter
        assert_eq!(unicode_to_evdev(0x0D), Some((28, false)));
    }

    #[test]
    fn test_unicode_to_evdev_symbols() {
        use super::unicode_to_evdev;

        assert_eq!(unicode_to_evdev(0x2D), Some((12, false))); // '-'
        assert_eq!(unicode_to_evdev(0x5F), Some((12, true))); // '_'
        assert_eq!(unicode_to_evdev(0x3D), Some((13, false))); // '='
        assert_eq!(unicode_to_evdev(0x2B), Some((13, true))); // '+'
        assert_eq!(unicode_to_evdev(0x5B), Some((26, false))); // '['
        assert_eq!(unicode_to_evdev(0x7B), Some((26, true))); // '{'
    }

    #[test]
    fn test_unicode_to_evdev_non_ascii_returns_none() {
        use super::unicode_to_evdev;

        // CJK characters should return None (handled by keysym path)
        assert_eq!(unicode_to_evdev(0x754C), None); // unicode codepoint beyond ASCII
        assert_eq!(unicode_to_evdev(0x4E16), None); // unicode codepoint beyond ASCII
        // Accented characters
        assert_eq!(unicode_to_evdev(0x00E9), None); // 'e' with acute
        // High values
        assert_eq!(unicode_to_evdev(0xD83D), None); // high surrogate
    }

    #[test]
    fn test_unicode_keysym_encoding() {
        // Verify the XKB Unicode keysym formula: 0x01000000 + code_point
        let unicode: u16 = 0x754C;
        let keysym = 0x0100_0000_u32 + u32::from(unicode);
        assert_eq!(keysym, 0x0100_754C);

        let unicode: u16 = 0x4E16;
        let keysym = 0x0100_0000_u32 + u32::from(unicode);
        assert_eq!(keysym, 0x0100_4E16);

        // ASCII 'A' would be 0x01000041, but we use evdev for ASCII
        let unicode: u16 = 0x0041;
        let keysym = 0x0100_0000_u32 + u32::from(unicode);
        assert_eq!(keysym, 0x0100_0041);
    }

    #[test]
    fn test_unicode_full_ascii_coverage() {
        use super::unicode_to_evdev;

        // Every printable ASCII character (0x20-0x7E) should map to something
        for cp in 0x20u16..=0x7E {
            assert!(
                unicode_to_evdev(cp).is_some(),
                "ASCII 0x{:02X} ('{}') should have a mapping",
                cp,
                char::from(cp as u8)
            );
        }
    }
}
