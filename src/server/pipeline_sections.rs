//! Fork-owned sections extracted from the `start_pipeline` frame loop.
//!
//! Every function here is a verbatim relocation of code authored by this
//! fork (verified per-hunk with `git blame` against the upstream merge-base
//! `2ca8422` — see each function's provenance note). The surrounding loop
//! body is ~82% upstream text; restructuring it would poison future
//! merges, so only fork-authored blocks move, and each call site keeps its
//! exact `continue`/fall-through shape.
//!
//! Style mirrors `process_cursor_update` in `display_handler.rs`: methods
//! on the handler, taking loop-owned state as explicit parameters, keeping
//! shared-state access (`self.*`) inside the object where it already lives.

use std::sync::Arc;

use tracing::{info, warn};

use super::display_handler::{LamcoDisplayHandler, VideoEncoder};
use crate::egfx::EncoderConfig as X264EncoderConfig;
#[cfg(feature = "x264")]
use crate::egfx::X264Encoder;

impl LamcoDisplayHandler {
    /// Compaction of hardware-padded capture strides to tight rows.
    ///
    /// Provenance: fork commit `5007122` ("normalize padded capture strides
    /// for odd width"), verified fork-authored across the whole block.
    ///
    /// Compositors negotiate row strides aligned to hardware limits (KWin:
    /// 256 bytes). For most modes width*4 is already aligned (1920*4=7680,
    /// 1600*4=6400, 1280*4=5120) — but 1366*4 = 5464 pads to 5632, and
    /// EVERY downstream CPU consumer (H.264 bgra_to_i420, bitmap
    /// convert_format, uncompressed WireToSurface1) assumes tight width*4
    /// rows. A padded stride therefore shears the H.264 picture (client
    /// tears down the EGFX DVC) and breaks the bitmap conversion fast path
    /// ("Unsupported conversion: BGRx -> BGRx"), which requires equal
    /// strides. Compact to tight rows ONCE here, right after
    /// materialization, so all consumers see the layout they assume.
    pub(crate) fn compact_padded_stride(frame: &mut lamco_pipewire::VideoFrame) {
        if let lamco_pipewire::FrameBuffer::Memory(data) = &frame.buffer {
            let tight = (frame.width as usize) * 4;
            if frame.stride as usize > tight
                && data.len() >= frame.stride as usize * frame.height as usize
            {
                static COMPACT_LOGS: std::sync::atomic::AtomicU32 =
                    std::sync::atomic::AtomicU32::new(0);
                let n = COMPACT_LOGS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n < 3 {
                    info!(
                        "Compacted padded stride: {} -> {} (width {})",
                        frame.stride, tight, frame.width
                    );
                }
                let mut packed = Vec::with_capacity(tight * frame.height as usize);
                for y in 0..frame.height as usize {
                    let row = &data[y * frame.stride as usize..][..tight];
                    packed.extend_from_slice(row);
                }
                frame.buffer = lamco_pipewire::FrameBuffer::Memory(Arc::new(packed));
                frame.stride = tight as u32;
            }
        }
    }

    /// Track per-frame capture-size changes from renegotiated streams.
    ///
    /// Provenance: fork commit `3cb1e20` ("honor the client's requested
    /// desktop size"), verified fork-authored across the whole block.
    ///
    /// During PipeWire renegotiation the stream can deliver frames at the
    /// OLD size for a while and then flaps to the new one. One-shot
    /// "first frame is truth" logic records the PRE-renegotiation size and
    /// then misses the real one: stale geometry then offsets input mapping
    /// into a capture size that no longer exists. Tracking per frame is
    /// one comparison and self-heals every transition.
    pub(crate) async fn track_capture_size_change(&self, frame: &lamco_pipewire::VideoFrame) {
        let (cw, ch) = *self.capture_size.read().await;
        if (frame.width, frame.height) != (cw, ch) {
            info!(
                "Capture size changed: {}x{} -> {}x{} (stream renegotiated)",
                cw, ch, frame.width, frame.height
            );
            self.update_capture_size(frame.width as u32, frame.height as u32)
                .await;
        }
    }

    /// Elastic-capture (kwin-virtual) resize path.
    ///
    /// Provenance: fork commit `3cb1e20` (elastic branch) + `a8ad82a`
    /// (kwin-virtual strategy), verified fork-authored across the whole
    /// block (the upstream DRM mode-switch scaffolding above it stays
    /// inline).
    ///
    /// The compositor-side virtual output is recreated at ANY requested
    /// size; the PipeWire stream rebinds to the new node. This replaces
    /// the DRM mode-switch path entirely — no mode list, no
    /// kscreen-doctor, identity scaling guaranteed (capture == desktop).
    ///
    /// Returns `true` when the elastic session handled this resize (the
    /// caller then `continue`s); `false` when no elastic session is bound
    /// and the upstream PipeWire Destroy/Create path must run instead.
    pub(crate) async fn handle_elastic_resize(&self, req_width: u16, req_height: u16) -> bool {
        let elastic = {
            let hook = self.elastic_capture.read();
            hook.clone()
        };
        let Some(session) = elastic else {
            return false;
        };
        info!(
            "Elastic capture: recreating virtual output at {}x{}",
            req_width, req_height
        );
        match session.resize_capture_source(req_width, req_height).await {
            Some((w, h)) => {
                // Rebind the PipeWire stream to the new node.
                // resize_capture_source already updated the session's
                // stream table; fetch the fresh node.
                let streams = session.streams();
                if let Some(s) = streams.first() {
                    let old_node = self.capture_node.load(std::sync::atomic::Ordering::Relaxed);
                    self.rebind_capture_node(old_node, s.node_id, s.width, s.height)
                        .await;
                }
                // Record capture truth; desktop stays at the client's
                // request (the stored size is updated by
                // request_initial_size on the next activation).
                info!("Elastic capture resized: source now delivers {}x{}", w, h);
            }
            None => {
                warn!(
                    "Elastic capture resize to {}x{} failed — keeping current stream",
                    req_width, req_height
                );
            }
        }
        true
    }

    /// x264 software-backend rung of the AVC420 encoder ladder.
    ///
    /// Provenance: fork commit `b62bcbd` ("x264 software encoder backend")
    /// — the two fork-authored ladder rungs relocated verbatim; the
    /// surrounding OpenH264/VA-API scaffolding is upstream text and stays
    /// at the call site.
    ///
    /// Tries x264 when the configured backend allows it ("auto" or
    /// explicit "x264") and nothing was built by an earlier rung. On
    /// success returns the built encoder; on failure (or backend not
    /// selected) returns `None` and the caller falls through to the
    /// OpenH264 rung. Diagnostics and periodic-IDR configuration are
    /// applied exactly as the inline code did.
    #[expect(
        unused_variables,
        reason = "parameters are only consumed on the x264 feature build"
    )]
    pub(crate) fn try_x264_backend(
        &self,
        config: &X264EncoderConfig,
        diagnostics: Option<Arc<crate::egfx::encode_diagnostics::EncodeDiagnostics>>,
        aligned_width: u16,
        aligned_height: u16,
        context: &str,
    ) -> Option<VideoEncoder> {
        #[cfg(feature = "x264")]
        {
            let backend = self.config.egfx.encoder_backend.to_lowercase();
            if backend == "x264" || backend == "auto" {
                match X264Encoder::new(config.clone()) {
                    Ok(mut encoder) => {
                        encoder.set_diagnostics(diagnostics.clone());
                        // Periodic full-frame IDR = the artifact self-heal
                        // for damage-hint misses (window-drag trails on
                        // zkde hints).
                        encoder.configure_periodic_idr(self.config.egfx.periodic_idr_interval);
                        info!(
                            "✅ x264 AVC420 encoder initialized for {}×{} ({})",
                            aligned_width, aligned_height, context
                        );
                        Some(VideoEncoder::X264(encoder))
                    }
                    Err(e) => {
                        warn!(
                            "Failed to create x264 encoder: {:?} - falling back to OpenH264",
                            e
                        );
                        None
                    }
                }
            } else {
                None
            }
        }
        #[cfg(not(feature = "x264"))]
        {
            // Keep the parameters referenced on non-x264 builds (they are
            // the ladder's shared inputs; the rung simply doesn't exist).
            let _ = (config, diagnostics, aligned_width, aligned_height, context);
            None
        }
    }
}
