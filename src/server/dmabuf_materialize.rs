//! DMA-BUF → CPU memory materialization for captured frames.
//!
//! When PipeWire negotiates DMA-BUF buffers but the active pipeline is
//! software (no hardware encoder import), the EGFX paths drop DmaBuf
//! frames entirely — producing a silent black screen on software-rendered
//! compositors (llvmpipe/kms_swrast negotiate DmaBuf fine but the capture
//! consumers are all CPU-side).
//!
//! Rather than dropping, materialize the frame into CPU memory here:
//! mmap the (linear) buffer, bracket the read with DMA_BUF_IOCTL_SYNC,
//! and hand the pipeline a `FrameBuffer::Memory` frame. If the mapping
//! genuinely reads zeros (virtual-GPUBacking with no CPU-coherent data),
//! that is counted and logged so the condition is observable.

use lamco_pipewire::{DmaBufDescriptor, FrameBuffer, VideoFrame};
use tracing::{info, trace, warn};

use crate::egfx::dmabuf_access;

/// FrameFlags::DMABUF bit (crate has set but no clear — clear via from_bits).
const FRAME_FLAG_DMABUF_BIT: u32 = 1 << 0;

/// Materialize a DmaBuf frame into CPU memory. Non-DmaBuf frames pass
/// through unchanged. Insert BEFORE the frame is cached so the cache only
/// ever holds CPU-resident data (cloning a DmaBuf loses the FD, and a
/// recycled buffer's contents aren't stable after PipeWire takes it back).
pub fn materialize_dmabuf_frame(mut frame: VideoFrame) -> VideoFrame {
    let desc = match &frame.buffer {
        FrameBuffer::DmaBuf(desc) => desc,
        FrameBuffer::Memory(_) => return frame,
    };

    match read_dmabuf_to_vec(desc) {
        Ok(data) => {
            static FIRST_MATERIALIZED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !FIRST_MATERIALIZED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                info!(
                    width = desc.width,
                    height = desc.height,
                    modifier = desc.modifier,
                    stride = desc.planes.first().map(|p| p.stride).unwrap_or(0),
                    bytes = data.len(),
                    "✅ DMA-BUF frame materialized to CPU memory (first frame; build has dmabuf_materialize)"
                );
            }

            // Buffer is now CPU-resident — clear the DMABUF flag so
            // downstream agents stop treating it as a GPU buffer.
            frame.flags =
                lamco_pipewire::FrameFlags::from_bits(frame.flags.bits() & !FRAME_FLAG_DMABUF_BIT);
            frame.buffer = FrameBuffer::Memory(std::sync::Arc::new(data));
            frame
        }
        Err(e) => {
            warn!(
                frame_id = frame.frame_id,
                width = desc.width,
                height = desc.height,
                modifier = desc.modifier,
                "DMA-BUF frame could not be materialized to CPU memory ({e}); dropping frame"
            );
            // Leave as DmaBuf: downstream paths drop/log it.
            frame
        }
    }
}

#[expect(unsafe_code, reason = "mmap/munmap required for DMA-BUF CPU access")]
fn read_dmabuf_to_vec(desc: &DmaBufDescriptor) -> Result<Vec<u8>, String> {
    dmabuf_access::ensure_linear(desc.modifier)?;

    if desc.planes.is_empty() {
        return Err("no planes".into());
    }

    let plane = &desc.planes[0];
    let size = (desc.height as usize).saturating_mul(plane.stride as usize);

    // Bounds-checked read: the helper maps offset+size and clamps to the
    // dma-buf's real extent (SIGBUS guard — reading past a dma-buf's end
    // aborts the process, it is not a catchable error).
    let vec = dmabuf_access::read_plane_to_vec(&plane.fd, plane.offset, size)?;

    let nonzero = dmabuf_access::dmabuf_stats::record(&vec);
    if !nonzero {
        warn!(
            width = desc.width,
            height = desc.height,
            modifier = desc.modifier,
            "DMA-BUF frame materialized but reads all-zero — exporter backing likely has no CPU-visible data"
        );
    } else {
        trace!(
            "DMA-BUF frame materialized to CPU memory ({} bytes)",
            vec.len()
        );
    }

    Ok(vec)
}

/// Log a summary of materialization stats; cheap to call periodically.
pub fn log_materialization_summary() {
    let (total, nonzero) = dmabuf_access::dmabuf_stats::snapshot();
    if total > 0 {
        info!(
            total,
            nonzero,
            zero_ratio_pct = (total - nonzero) * 100 / total,
            "DMA-BUF materialization stats"
        );
    }
}
