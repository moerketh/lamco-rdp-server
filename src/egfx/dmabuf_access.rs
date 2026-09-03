//! DMA-BUF CPU access helpers.
//!
//! Reading a dma-buf via mmap requires bracketing the access with the
//! `DMA_BUF_IOCTL_SYNC` ioctl (`struct dma_buf_sync` with
//! `DMA_BUF_SYNC_START` / `DMA_BUF_SYNC_END`). That ioctl is what tells the
//! exporter to flush/invalidate caches so the CPU mapping is coherent;
//! skipping it is outside the dma-buf contract and legitimately returns
//! stale or zero pages — notably on software renderers (kms_swrast,
//! llvmpipe) where the backing shmem is rendered without CPU cache
//! coherency for external mappers.
//!
//! Kernel uapi (include/uapi/linux/dma-buf.h):
//! ```c
//! struct dma_buf_sync { __u64 flags; };
//! #define DMA_BUF_SYNC_READ      (1 << 0)
//! #define DMA_BUF_SYNC_WRITE     (2 << 0)
//! #define DMA_BUF_SYNC_RW        (DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE)
//! #define DMA_BUF_SYNC_START     (0 << 2)
//! #define DMA_BUF_SYNC_END       (1 << 2)
//! #define DMA_BUF_BASE           'b'
//! #define DMA_BUF_IOCTL_SYNC    _IOW(DMA_BUF_BASE, 0, struct dma_buf_sync)
//! ```
//! `DMA_BUF_IOCTL_SYNC` = 0x40086200 on all supported architectures
//! (dir/write=1 in bits 31..30, size=8 in bits 29..16, 'b'=0x62, nr=0).

use std::os::fd::AsRawFd;

use tracing::{debug, trace, warn};

/// `struct dma_buf_sync` — a single `__u64 flags` field on the wire.
#[derive(Debug, Clone, Copy)]
struct DmaBufSync {
    flags: u64,
}

const DMA_BUF_SYNC_READ: u64 = 1 << 0;
const DMA_BUF_SYNC_START: u64 = 0 << 2;
const DMA_BUF_SYNC_END: u64 = 1 << 2;

/// `_IOW(b, 0, struct dma_buf_sync)` as an unsigned long ioctl request.
/// 0x4008_6200: the size field is 8 because `struct dma_buf_sync` is a
/// single `__u64`. (0x4004_6200 encodes size=4 and would get ENOTTY.)
/// Verified by compiling the macro from linux/dma-buf.h.
const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x4008_6200;

/// DRM_FORMAT_MOD_LINEAR — the only layout this CPU read path supports.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// RAII guard bracketing a CPU read of a dma-buf: issues
/// `DMA_BUF_IOCTL_SYNC` + `DMA_BUF_SYNC_START` on creation and the
/// matching `DMA_BUF_SYNC_END` on drop.
pub struct DmaBufSyncGuard<'fd> {
    fd: &'fd std::os::fd::OwnedFd,
}

/// Issue one `DMA_BUF_IOCTL_SYNC` with the given flags. Failure is
/// returned to the caller, who decides whether it is fatal.
#[expect(
    unsafe_code,
    reason = "raw ioctl syscall required; no safe wrapper exists in our dependency set"
)]
fn dma_buf_sync(fd: &std::os::fd::OwnedFd, flags: u64) -> std::io::Result<()> {
    let sync = DmaBufSync { flags };
    let ret = unsafe { libc::ioctl(fd.as_raw_fd(), DMA_BUF_IOCTL_SYNC, &sync) };
    if ret == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl<'fd> DmaBufSyncGuard<'fd> {
    /// Begin a CPU read access section on the dma-buf.
    ///
    /// A failed START is logged but non-fatal: exporters that reject the
    /// ioctl can still be mmap-coherent, so we proceed with a warning
    /// rather than failing the whole frame.
    pub fn begin_read(fd: &'fd std::os::fd::OwnedFd) -> Self {
        if let Err(e) = dma_buf_sync(fd, DMA_BUF_SYNC_READ | DMA_BUF_SYNC_START) {
            warn!(
                "DMA_BUF_IOCTL_SYNC START failed ({e}) — exporter may not support sync; reads may be stale"
            );
        }
        Self { fd }
    }
}

impl Drop for DmaBufSyncGuard<'_> {
    fn drop(&mut self) {
        if let Err(e) = dma_buf_sync(self.fd, DMA_BUF_SYNC_READ | DMA_BUF_SYNC_END) {
            trace!("DMA_BUF_IOCTL_SYNC END failed ({e})");
        }
    }
}

/// Frame-content instrumentation shared across DMA-BUF read paths.
/// Counts mapped frames that contained at least one non-zero byte —
/// the measurement that distinguishes "reads zeros" from "renders wrong".
pub mod dmabuf_stats {
    use std::sync::atomic::{AtomicU64, Ordering};

    static FRAMES_TOTAL: AtomicU64 = AtomicU64::new(0);
    static FRAMES_NONZERO: AtomicU64 = AtomicU64::new(0);

    /// Record one frame; returns whether it contained any non-zero byte.
    pub fn record(data: &[u8]) -> bool {
        FRAMES_TOTAL.fetch_add(1, Ordering::Relaxed);
        let nonzero = data.iter().any(|&b| b != 0);
        if nonzero {
            FRAMES_NONZERO.fetch_add(1, Ordering::Relaxed);
        }
        nonzero
    }

    pub fn snapshot() -> (u64, u64) {
        (
            FRAMES_TOTAL.load(Ordering::Relaxed),
            FRAMES_NONZERO.load(Ordering::Relaxed),
        )
    }
}

/// Count of frames whose mapped plane contained at least one non-zero byte.
/// Instrumentation for diagnosing zero-read ("black screen") captures.
#[derive(Debug, Default)]
pub struct NonZeroFrameStats {
    pub frames_total: u64,
    pub frames_nonzero: u64,
}

impl NonZeroFrameStats {
    pub fn record(&mut self, data: &[u8]) -> bool {
        self.frames_total += 1;
        let nonzero = data.iter().any(|&b| b != 0);
        if nonzero {
            self.frames_nonzero += 1;
        } else {
            debug!(
                "dmabuf frame appears all-zero (frame #{})",
                self.frames_total
            );
        }
        nonzero
    }
}

/// Reject non-linear layouts: this path does a flat `height * stride` copy,
/// which is only correct for `DRM_FORMAT_MOD_LINEAR` (or INVALID-treated-
/// as-linear). A tiled modifier would silently produce garbage.
pub fn ensure_linear(modifier: u64) -> Result<(), &'static str> {
    if modifier == DRM_FORMAT_MOD_LINEAR {
        Ok(())
    } else {
        Err("non-linear DMA-BUF modifier not supported by CPU read path")
    }
}
