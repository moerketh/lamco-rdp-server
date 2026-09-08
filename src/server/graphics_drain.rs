//! Graphics Frame Drain Task
//!
//! Implements Phase 1 of the event multiplexer: Graphics queue with drop/coalesce policy.
//! This prevents graphics congestion from blocking input, control, or clipboard events.
//!
//! # Architecture
//!
//! ```text
//! PipeWire Thread
//!      │
//!      ├─> Display Handler (frame processing)
//!      │
//!      ▼
//! Graphics Queue (bounded 4, try_send with drop)
//!      │
//!      ├─> Graphics Drain Task (this module)
//!      │     └─> Coalesce multiple queued frames → keep only latest
//!      │
//!      ▼
//! IronRDP DisplayUpdate Channel
//!      │
//!      └─> RemoteFX encoding → RDP client
//! ```
//!
//! # QoS Benefits
//!
//! - **Non-blocking:** PipeWire never blocks on full queue (try_send)
//! - **Coalescing:** Multiple queued frames reduced to single latest frame
//! - **Isolation:** Graphics congestion cannot affect other subsystems
//! - **Statistics:** Track drops and coalescing for monitoring

use std::sync::Arc;

use ironrdp_server::DisplayUpdate;
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

use crate::server::event_multiplexer::GraphicsFrame;

/// Statistics for graphics drain task
#[derive(Debug, Clone, Default)]
pub(super) struct GraphicsDrainStats {
    /// Total frames received from queue
    pub frames_received: u64,
    /// Frames coalesced (dropped because newer frame available)
    pub frames_coalesced: u64,
    /// Frames sent to IronRDP
    pub frames_sent: u64,
    /// Frames dropped because the DisplayUpdate channel was full
    /// (dead peer not draining)
    pub frames_dropped: u64,
}

/// Start the graphics drain task
///
/// This task continuously drains the graphics queue, coalescing multiple frames
/// into a single latest frame, then converts to IronRDP format and sends.
pub(super) fn start_graphics_drain_task(
    mut graphics_rx: mpsc::Receiver<GraphicsFrame>,
    update_sender: Arc<tokio::sync::Mutex<mpsc::Sender<DisplayUpdate>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("🎬 Graphics drain task started (Phase 1 multiplexer)");
        let mut stats = GraphicsDrainStats::default();

        loop {
            let mut latest_frame = match graphics_rx.recv().await {
                Some(frame) => {
                    stats.frames_received += 1;
                    trace!(
                        "📥 Graphics queue: received frame {}",
                        stats.frames_received
                    );
                    frame
                }
                None => {
                    info!("Graphics channel closed, drain task exiting");
                    break;
                }
            };

            // Coalesce: Drain any additional queued frames, keep only latest
            let mut coalesced_count = 0u32;
            while let Ok(newer_frame) = graphics_rx.try_recv() {
                stats.frames_received += 1;
                coalesced_count += 1;
                latest_frame = newer_frame;
            }

            if coalesced_count > 0 {
                stats.frames_coalesced += coalesced_count as u64;
                trace!(
                    "🔄 Graphics queue: coalesced {} frames (keeping latest)",
                    coalesced_count
                );
                if stats.frames_coalesced % 100 == 0 {
                    info!(
                        "📊 Graphics coalescing: {} frames coalesced total",
                        stats.frames_coalesced
                    );
                }
            }

            // Send already-converted IronBitmapUpdate directly (no double conversion!)
            let update = DisplayUpdate::Bitmap(latest_frame.iron_bitmap);

            // try_send, never .await: the MutexGuard is held across this
            // statement, and an awaited send on a full channel (dead peer
            // stops draining it) would park this task holding the lock and
            // wedge every other display path. Drop on Full (the coalescing
            // policy already favors the latest frame; a dropped frame is
            // repainted by the next one's damage regions), bail on Closed.
            {
                let sender = update_sender.lock().await;
                match sender.try_send(update) {
                    Ok(()) => {
                        stats.frames_sent += 1;
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        warn!("DisplayUpdate channel closed — drain task exiting");
                        return;
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        stats.frames_dropped += 1;
                        static DRAIN_DROP_LOG: std::sync::atomic::AtomicU64 =
                            std::sync::atomic::AtomicU64::new(0);
                        let n = DRAIN_DROP_LOG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if n.is_multiple_of(60) {
                            warn!(
                                "DisplayUpdate channel full — dropping coalesced frame \
                                 ({} drops so far; client not draining?)",
                                n
                            );
                        }
                    }
                }
            }

            if stats.frames_sent % 100 == 0 {
                debug!(
                    "📊 Graphics drain stats: received={}, coalesced={}, sent={}, dropped={}",
                    stats.frames_received,
                    stats.frames_coalesced,
                    stats.frames_sent,
                    stats.frames_dropped
                );
            }
        }

        info!(
            "📊 Graphics drain task final stats: received={}, coalesced={}, sent={}, dropped={}",
            stats.frames_received, stats.frames_coalesced, stats.frames_sent, stats.frames_dropped
        );
    })
}

// Removed convert_to_iron_format - no longer needed!
// GraphicsFrame now contains pre-converted IronBitmapUpdate
// This eliminates double conversion overhead
