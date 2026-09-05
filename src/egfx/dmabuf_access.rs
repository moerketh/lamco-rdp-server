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

/// Read `len` bytes of a dma-buf plane into a `Vec`, bounds-checked.
///
/// The one correct pattern used by every CPU-read site in this crate:
/// mmap the buffer (pgoff 0 — dma-buf mmap does not accept a byte offset),
/// bracket with `DMA_BUF_IOCTL_SYNC`, and copy out `plane.offset..+len`.
/// This helper is the single place that enforces the two invariants the
/// previous per-site copies got wrong or omitted:
///
/// 1. The mapping must cover `plane.offset + len` bytes. Mapping exactly
///    `len` bytes and then reading from `ptr + plane.offset` reads past
///    the end of the mapping whenever `plane.offset > 0` (common for
///    packed/multi-plane exporters) — SIGBUS on a real dma-buf whose
///    mapping ends at the buffer extent.
/// 2. The read must stay within the dma-buf's actual size
///    (`lseek(fd, 0, SEEK_END)`), not a stride-derived guess. A size
///    computed as `height * stride` (or `.max(w*h*4)`) can exceed what
///    the exporter allocated; reading past a dma-buf's extent also
///    raises SIGBUS, which aborts the whole process — not catchable.
///
/// The mapping length is page-rounded up (mmap requires page granularity)
/// and `len` is clamped to `dmabuf_size - plane.offset` so the copy is
/// always in-bounds of both the mapping and the underlying object.
#[expect(
    unsafe_code,
    reason = "mmap/munmap and the bounds-checked copy require raw pointers"
)]
pub fn read_plane_to_vec(
    fd: &std::os::fd::OwnedFd,
    plane_offset: u32,
    len: usize,
) -> Result<Vec<u8>, String> {
    use std::num::NonZeroUsize;
    use std::os::fd::BorrowedFd;

    use nix::sys::mman::{MapFlags, ProtFlags, mmap, munmap};

    if len == 0 {
        return Err("zero-length plane read".into());
    }

    // Invariant 2: never read past the dma-buf's real extent. SEEK_END on a
    // dma-buf fd reports the exporter-allocated size.
    let dmabuf_size = nix::unistd::lseek(fd, 0, nix::unistd::Whence::SeekEnd)
        .map_err(|e| format!("dma-buf lseek(SEEK_END) failed: {e}"))?
        as usize;
    if dmabuf_size == 0 {
        return Err("dma-buf reports zero size".into());
    }
    let plane_offset = plane_offset as usize;
    if plane_offset >= dmabuf_size {
        return Err(format!(
            "plane offset {plane_offset} beyond dma-buf size {dmabuf_size}"
        ));
    }
    let len = len.min(dmabuf_size - plane_offset);
    if len == 0 {
        return Err("plane read length clamped to zero".into());
    }

    // Invariant 1: cover offset+len. Round the mapping up to page size —
    // mmap rejects non-page-multiple lengths.
    const PAGE_SIZE: usize = 4096;
    // len ≥ 1 (validated above) ⇒ pages ≥ 1 ⇒ the rounded length is at
    // least PAGE_SIZE, so the NonZeroUsize conversion cannot fail; the
    // unwrap_or arm is unreachable and exists only to satisfy the type.
    let nz_map_len = NonZeroUsize::new((plane_offset + len).div_ceil(PAGE_SIZE) * PAGE_SIZE)
        .unwrap_or(NonZeroUsize::MIN);

    // SAFETY: fd is a valid OwnedFd (dup'd by lamco-pipewire). Mapping is
    // read-only and unmapped after the copy below.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd.as_raw_fd()) };
    // SAFETY: arguments validated above; length is page-multiple and nonzero.
    let ptr = unsafe {
        mmap(
            None,
            nz_map_len,
            ProtFlags::PROT_READ,
            MapFlags::MAP_SHARED,
            borrowed,
            0,
        )
    }
    .map_err(|e| format!("dma-buf mmap failed: {e}"))?;

    let sync = DmaBufSyncGuard::begin_read(fd);

    // SAFETY: ptr is valid for nz_map_len >= plane_offset+len bytes;
    // the copy reads exactly `len` bytes starting at plane_offset.
    let src = unsafe { ptr.as_ptr().add(plane_offset) as *const u8 };
    let mut vec = Vec::with_capacity(len);
    unsafe {
        std::ptr::copy_nonoverlapping(src, vec.as_mut_ptr(), len);
        vec.set_len(len);
    }

    drop(sync);
    // SAFETY: unmap exactly what was mapped, after the copy is done.
    unsafe {
        let _ = munmap(ptr, nz_map_len.get());
    }

    Ok(vec)
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

#[cfg(test)]
mod read_plane_tests {
    use super::*;

    /// A memfd standing in for a dma-buf: mmap-able, sized by its content.
    /// (A real dma-buf's `mmap` goes through the exporter's fault handler,
    /// but the bounds logic under test — lseek extent, offset+length
    /// mapping, clamping — is fd-type-agnostic.)
    fn fake_dmabuf(contents: &[u8]) -> std::os::fd::OwnedFd {
        use std::os::fd::AsFd as _;

        use nix::sys::memfd::{MFdFlags, MemFdCreateFlag, memfd_create};

        let flags = MFdFlags::from_bits(
            MemFdCreateFlag::MFD_CLOEXEC.bits() | MemFdCreateFlag::MFD_ALLOW_SEALING.bits(),
        )
        .expect("valid flags");
        let fd = memfd_create("fake-dmabuf", flags).expect("memfd_create");
        let mut written = 0;
        while written < contents.len() {
            let n = nix::unistd::write(fd.as_fd(), &contents[written..]).expect("write");
            assert!(n > 0, "zero-length write");
            written += n;
        }
        fd
    }

    #[test]
    fn reads_full_buffer_at_zero_offset() {
        let contents: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
        let fd = fake_dmabuf(&contents);
        let out = read_plane_to_vec(&fd, 0, contents.len()).expect("read");
        assert_eq!(out, contents);
    }

    #[test]
    fn reads_with_nonzero_plane_offset_in_bounds() {
        // Regression: the per-site code mapped `len` bytes at offset 0 and
        // then read from ptr+plane.offset — out of bounds of the mapping
        // whenever plane.offset > 0. With a 256-byte offset the read must
        // return the last 128 bytes, not SIGBUS.
        let contents: Vec<u8> = (0..384u32).map(|i| 0xA0 ^ i as u8).collect();
        let fd = fake_dmabuf(&contents);
        let out = read_plane_to_vec(&fd, 256, 128).expect("read with offset");
        assert_eq!(out.len(), 128);
        assert_eq!(out, &contents[256..384]);
    }

    #[test]
    fn clamps_length_to_dmabuf_extent() {
        // Request more than the buffer holds: must clamp, not read past the
        // end (the SIGBUS case on a real dma-buf).
        let contents = vec![0x5Au8; 1000];
        let fd = fake_dmabuf(&contents);
        let out = read_plane_to_vec(&fd, 0, 10_000).expect("clamped read");
        assert_eq!(out.len(), 1000);
        assert_eq!(out, &contents[..]);
    }

    #[test]
    fn rejects_offset_beyond_extent() {
        let fd = fake_dmabuf(&[0u8; 4096]);
        let err = read_plane_to_vec(&fd, 8192, 64).expect_err("must reject");
        assert!(err.contains("beyond dma-buf size"), "err: {err}");
    }

    #[test]
    fn rejects_zero_length() {
        let fd = fake_dmabuf(&[0u8; 4096]);
        let err = read_plane_to_vec(&fd, 0, 0).expect_err("must reject");
        assert!(err.contains("zero-length"), "err: {err}");
    }

    #[test]
    fn offset_plus_length_spanning_pages_is_mapped_whole() {
        // offset 4090, length 100 → spans the page boundary at 4096; the
        // mapping must cover 4190 (rounded to 2 pages) for the copy to be
        // in-bounds.
        let contents: Vec<u8> = (0..8192u32).map(|i| i as u8).collect();
        let fd = fake_dmabuf(&contents);
        let out = read_plane_to_vec(&fd, 4090, 100).expect("page-spanning read");
        assert_eq!(out, &contents[4090..4190]);
    }
}
