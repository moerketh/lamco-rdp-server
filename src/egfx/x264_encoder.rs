//! H.264/AVC420 Encoder using x264
//!
//! This module provides H.264 encoding using the x264 library for use with the
//! EGFX AVC420 codec. x264 is significantly faster than OpenH264 for real-time
//! encoding, especially with the `ultrafast` preset and `zerolatency` tune.
//!
//! # Performance
//!
//! x264 with `ultrafast` + `zerolatency` is 2-3× faster than OpenH264 for
//! real-time screen content encoding. On a 2-vCPU VM at 1920×1080:
//! - OpenH264: ~16-45ms per frame
//! - x264 ultrafast/zerolatency: ~5-12ms per frame
//!
//! # x264 vs OpenH264
//!
//! - x264 only supports AVC420 (4:2:0), NOT AVC444 (4:4:4)
//! - When AVC444 is negotiated, the OpenH264 encoder is used instead
//! - x264 outputs Annex B format natively (same as OpenH264)
//! - x264 handles BGRA→YUV420 conversion internally via `X264_CSP_BGRA`
//!
//! # FFI Safety
//!
//! x264 is loaded dynamically at runtime via `libloading`. The `x264_param_t`
//! and `x264_picture_t` structs are accessed via raw byte buffers with known
//! offsets (verified at compile time against the x264 164 ABI). Parameters are
//! set via the string-based `x264_param_parse` API, which is ABI-stable across
//! x264 versions. The picture is initialized via `x264_picture_init`.

#![expect(
    unsafe_code,
    reason = "FFI bindings to libx264 C library require unsafe"
)]

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};

use libloading::Library;
use thiserror::Error;
use tracing::{debug, info, warn};

use super::encoder::{EncoderConfig, EncoderError, EncoderResult, H264Frame};

// When the `h264` feature is also enabled, we can reuse OpenH264's optimized
// BGRA→YUV420 conversion (YUVBuffer::from_rgb_source) instead of our naive
// per-pixel loop. This is the same conversion the OpenH264 encoder path uses.
#[cfg(feature = "h264")]
use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};

// ─── x264 C API constants ───────────────────────────────────────────────────

/// Input colorspace: I420 (YUV 4:2:0 planar)
/// x264_csp_e: X264_CSP_I420 = 0x0002 (from x264.h)
/// We convert BGRA→I420 in Rust before feeding to x264, because x264's
/// internal BGRA conversion produces 4:4:4 (High 4:4:4 Predictive profile)
/// which mstsc cannot decode in AVC420 mode. Feeding I420 produces
/// Constrained Baseline profile (4:2:0) which mstsc handles correctly.
const X264_CSP_I420: c_int = 0x0002;

/// Force IDR keyframe on next encode (x264_type_e: NAL_SLICE_IDR = 5)
/// Actually, x264_picture_t.i_type uses X264_TYPE_IDR = 3
/// (x264 defines X264_TYPE_AUTO=0, X264_TYPE_IDR=3, etc. — NOT the NAL types)
const X264_TYPE_IDR: c_int = 3;
const X264_TYPE_AUTO: c_int = 0;

// ─── Struct sizes (verified against x264 164 ABI) ───────────────────────────

/// sizeof(x264_param_t) = 1024 bytes on x86_64 with x264 build 164
const X264_PARAM_SIZE: usize = 1024;
/// sizeof(x264_picture_t) = 240 bytes on x86_64
const X264_PICTURE_SIZE: usize = 240;
/// sizeof(x264_nal_t) = 40 bytes on x86_64
const X264_NAL_SIZE: usize = 40;

// ─── x264_picture_t field offsets (verified at runtime) ─────────────────────
//
// Layout (x86_64, x264 build 164):
//   offset 0:   int     i_type         (4 bytes)
//   offset 4:   int     i_qpplus1      (4 bytes)
//   offset 8:   int     i_pic_struct   (4 bytes)
//   offset 12:  int     b_keyframe     (4 bytes)
//   offset 16:  int64_t i_pts          (8 bytes)
//   offset 24:  int64_t i_dts          (8 bytes)
//   offset 32:  void*   param          (8 bytes, pointer to x264_param_t)
//   offset 40:  x264_image_t img       (56 bytes)
//     offset 40: int   i_csp           (4 bytes)
//     offset 44: int   i_plane         (4 bytes)
//     offset 48: int   i_stride[4]     (16 bytes)
//     offset 64: void* plane[4]        (32 bytes)
//   offset 96:  x264_image_properties_t prop
//   ... (rest doesn't matter for input)
const PIC_OFF_I_TYPE: usize = 0;
const PIC_OFF_I_PTS: usize = 16;
const PIC_OFF_PARAM: usize = 32;
const PIC_OFF_IMG: usize = 40;
const IMG_OFF_I_CSP: usize = PIC_OFF_IMG; // = 40
const IMG_OFF_I_PLANE: usize = PIC_OFF_IMG + 4; // = 44
const IMG_OFF_I_STRIDE: usize = PIC_OFF_IMG + 8; // = 48
const IMG_OFF_PLANE: usize = PIC_OFF_IMG + 24; // = 64

// ─── x264_nal_t field offsets ───────────────────────────────────────────────
//
// Layout (x86_64, x264 build 164):
//   offset 0:  int     i_ref_idc      (4 bytes)
//   offset 4:  int     i_type         (4 bytes)  — NAL type (5 = IDR)
//   offset 8:  int     b_long_startcode (4 bytes)
//   offset 12: int     i_first_mb     (4 bytes)
//   offset 16: int     i_last_mb      (4 bytes)
//   offset 20: int     i_payload      (4 bytes)  — NOTE: this is i_payload, not i_size
//   offset 24: uint8_t* p_payload     (8 bytes)
//   offset 32: int     i_padding      (4 bytes)
const NAL_OFF_I_TYPE: usize = 4;
const NAL_OFF_I_PAYLOAD: usize = 20;
const NAL_OFF_P_PAYLOAD: usize = 24;

// ─── Function pointer types ─────────────────────────────────────────────────

type X264ParamDefaultFn = unsafe extern "C" fn(*mut c_void);
type X264ParamDefaultPresetFn =
    unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
type X264ParamParseFn =
    unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
type X264ParamCleanupFn = unsafe extern "C" fn(*mut c_void);
type X264PictureInitFn = unsafe extern "C" fn(*mut c_void);
type X264EncoderOpenFn = unsafe extern "C" fn(*const c_void) -> *mut c_void;
type X264EncoderEncodeFn = unsafe extern "C" fn(
    *mut c_void,
    *mut *mut u8,    // x264_nal_t** pp_nal
    *mut c_int,      // int* pi_nal
    *const u8,       // x264_picture_t* pic_in (raw byte buffer)
    *mut u8,         // x264_picture_t* pic_out (raw byte buffer, can be NULL)
) -> c_int;
type X264EncoderCloseFn = unsafe extern "C" fn(*mut c_void);
type X264EncoderDelayFn = unsafe extern "C" fn(*mut c_void) -> c_int;

// ─── Loaded x264 API ────────────────────────────────────────────────────────

struct X264Api {
    _lib: Library,
    param_default: X264ParamDefaultFn,
    param_default_preset: X264ParamDefaultPresetFn,
    param_parse: X264ParamParseFn,
    param_cleanup: X264ParamCleanupFn,
    picture_init: X264PictureInitFn,
    encoder_open: X264EncoderOpenFn,
    encoder_encode: X264EncoderEncodeFn,
    encoder_close: X264EncoderCloseFn,
    encoder_delay: X264EncoderDelayFn,
}

impl std::fmt::Display for X264Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "x264 FFI (dynamic loading)")
    }
}

fn load_x264_api() -> Result<X264Api, String> {
    unsafe {
        // Try common library names (versioned and unversioned)
        let lib = Library::new("libx264.so.164")
            .or_else(|_| Library::new("libx264.so.148"))
            .or_else(|_| Library::new("libx264.so"))
            .or_else(|_| {
                Library::new("/usr/lib/x86_64-linux-gnu/libx264.so.164")
            })
            .or_else(|_| {
                Library::new("/usr/lib/x86_64-linux-gnu/libx264.so")
            })
            .map_err(|e| {
                format!(
                    "Failed to load libx264.so: {e}. \
                     Install libx264-dev or libx264-164."
                )
            })?;

        let param_default: X264ParamDefaultFn = {
            let sym = lib.get::<X264ParamDefaultFn>(b"x264_param_default")
                .map_err(|e| format!("x264_param_default not found: {e}"))?;
            *sym
        };
        let param_default_preset: X264ParamDefaultPresetFn = {
            let sym = lib.get::<X264ParamDefaultPresetFn>(b"x264_param_default_preset")
                .map_err(|e| format!("x264_param_default_preset not found: {e}"))?;
            *sym
        };
        let param_parse: X264ParamParseFn = {
            let sym = lib.get::<X264ParamParseFn>(b"x264_param_parse")
                .map_err(|e| format!("x264_param_parse not found: {e}"))?;
            *sym
        };
        let param_cleanup: X264ParamCleanupFn = {
            let sym = lib.get::<X264ParamCleanupFn>(b"x264_param_cleanup")
                .map_err(|e| format!("x264_param_cleanup not found: {e}"))?;
            *sym
        };
        let picture_init: X264PictureInitFn = {
            let sym = lib.get::<X264PictureInitFn>(b"x264_picture_init")
                .map_err(|e| format!("x264_picture_init not found: {e}"))?;
            *sym
        };

        // x264_encoder_open is a macro: x264_encoder_open_##X264_BUILD
        // Try the versioned symbol first, then the plain name
        let encoder_open: X264EncoderOpenFn = {
            let sym = lib.get::<X264EncoderOpenFn>(b"x264_encoder_open_164")
                .or_else(|_| lib.get::<X264EncoderOpenFn>(b"x264_encoder_open_148"))
                .or_else(|_| lib.get::<X264EncoderOpenFn>(b"x264_encoder_open"))
                .map_err(|e| format!("x264_encoder_open not found: {e}"))?;
            *sym
        };
        let encoder_encode: X264EncoderEncodeFn = {
            let sym = lib.get::<X264EncoderEncodeFn>(b"x264_encoder_encode")
                .map_err(|e| format!("x264_encoder_encode not found: {e}"))?;
            *sym
        };
        let encoder_close: X264EncoderCloseFn = {
            let sym = lib.get::<X264EncoderCloseFn>(b"x264_encoder_close")
                .map_err(|e| format!("x264_encoder_close not found: {e}"))?;
            *sym
        };
        let encoder_delay: X264EncoderDelayFn = {
            let sym = lib.get::<X264EncoderDelayFn>(b"x264_encoder_delayed_frames")
                .map_err(|e| format!("x264_encoder_delayed_frames not found: {e}"))?;
            *sym
        };

        Ok(X264Api {
            _lib: lib,
            param_default,
            param_default_preset,
            param_parse,
            param_cleanup,
            picture_init,
            encoder_open,
            encoder_encode,
            encoder_close,
            encoder_delay,
        })
    }
}

/// Get the loaded x264 API, loading it on first call.
fn get_x264_api() -> Result<&'static X264Api, String> {
    static API: std::sync::OnceLock<Result<X264Api, String>> = std::sync::OnceLock::new();
    let result = API.get_or_init(load_x264_api);
    match result {
        Ok(api) => Ok(api),
        Err(e) => Err(e.clone()),
    }
}

// ─── Helper: write to raw byte buffer at offset ─────────────────────────────

#[inline]
unsafe fn write_i32_at(buf: *mut u8, offset: usize, val: c_int) {
    unsafe {
        std::ptr::write_unaligned(buf.add(offset) as *mut c_int, val);
    }
}

#[inline]
unsafe fn write_i64_at(buf: *mut u8, offset: usize, val: i64) {
    unsafe {
        std::ptr::write_unaligned(buf.add(offset) as *mut i64, val);
    }
}

#[inline]
unsafe fn write_ptr_at(buf: *mut u8, offset: usize, val: *const u8) {
    unsafe {
        std::ptr::write_unaligned(buf.add(offset) as *mut *const u8, val);
    }
}

#[inline]
unsafe fn read_i32_at(buf: *const u8, offset: usize) -> c_int {
    unsafe { std::ptr::read_unaligned(buf.add(offset) as *const c_int) }
}

#[inline]
unsafe fn read_ptr_at(buf: *const u8, offset: usize) -> *const u8 {
    unsafe {
        std::ptr::read_unaligned(buf.add(offset) as *const *const u8)
    }
}

// ─── X264Encoder ────────────────────────────────────────────────────────────

/// H.264 encoder using x264 (AVC420 only)
///
/// # Feature Gate
///
/// Requires the `x264` feature to be enabled.
pub struct X264Encoder {
    api: &'static X264Api,
    encoder: *mut c_void,
    /// Raw byte buffer for x264_param_t (1024 bytes)
    param_buf: Vec<u8>,
    /// Raw byte buffer for x264_picture_t (240 bytes) — input picture
    pic_in_buf: Vec<u8>,
    /// Pre-allocated Y plane buffer for BGRA→I420 conversion
    y_plane: Vec<u8>,
    /// Pre-allocated U plane buffer for BGRA→I420 conversion
    u_plane: Vec<u8>,
    /// Pre-allocated V plane buffer for BGRA→I420 conversion
    v_plane: Vec<u8>,
    config: EncoderConfig,
    frame_count: u64,
    width: u32,
    height: u32,
    /// Whether the next frame should be forced to IDR
    force_idr: bool,
    /// Whether we need to (re)initialize the encoder for new dimensions
    needs_reinit: bool,
    diagnostics: Option<std::sync::Arc<super::encode_diagnostics::EncodeDiagnostics>>,
}

unsafe impl Send for X264Encoder {}

#[derive(Debug, Error)]
enum X264InitError {
    #[error("x264 library load failed: {0}")]
    LoadFailed(String),
    #[error("x264 param default preset failed: {0}")]
    PresetFailed(c_int),
    #[error("x264 encoder open failed")]
    EncoderOpenFailed,
    #[error("x264 param parse failed for '{key}={value}': {code}")]
    ParamParseFailed { key: String, value: String, code: c_int },
}

impl X264Encoder {
    /// Create a new x264 encoder.
    ///
    /// The encoder is lazily initialized on the first `encode_bgra` call
    /// when the actual frame dimensions are known.
    pub fn new(config: EncoderConfig) -> EncoderResult<Self> {
        let api = get_x264_api().map_err(|e| {
            EncoderError::InitFailed(format!("x264 library load failed: {e}"))
        })?;

        info!("x264 encoder backend initialized (ultrafast/zerolatency)");

        Ok(Self {
            api,
            encoder: std::ptr::null_mut(),
            param_buf: vec![0u8; X264_PARAM_SIZE],
            pic_in_buf: vec![0u8; X264_PICTURE_SIZE],
            y_plane: Vec::new(),
            u_plane: Vec::new(),
            v_plane: Vec::new(),
            config,
            frame_count: 0,
            width: 0,
            height: 0,
            force_idr: true, // First frame must be IDR
            needs_reinit: true,
            diagnostics: None,
        })
    }

    /// Attach encoder diagnostics (same interface as Avc420Encoder).
    pub fn set_diagnostics(
        &mut self,
        diagnostics: Option<std::sync::Arc<super::encode_diagnostics::EncodeDiagnostics>>,
    ) {
        self.diagnostics = diagnostics;
    }

    /// (Re)initialize the x264 encoder for the given dimensions.
    ///
    /// Uses `x264_param_default_preset("ultrafast", "zerolatency")` then
    /// overrides key parameters via `x264_param_parse` for ABI stability.
    fn reinit(&mut self, width: u32, height: u32) -> EncoderResult<()> {
        unsafe {
            // Close existing encoder if open
            if !self.encoder.is_null() {
                (self.api.encoder_close)(self.encoder);
                self.encoder = std::ptr::null_mut();
            }

            // Zero the param buffer and set defaults
            self.param_buf.fill(0);
            let param = self.param_buf.as_mut_ptr() as *mut c_void;

            // Set defaults with preset + tune
            // ultrafast: fastest encoding, minimal analysis
            // zerolatency: no B-frames, no lookahead, single-frame pipeline
            let preset = CString::new("ultrafast").unwrap();
            let tune = CString::new("zerolatency").unwrap();
            let ret = (self.api.param_default_preset)(param, preset.as_ptr(), tune.as_ptr());
            if ret != 0 {
                return Err(EncoderError::InitFailed(format!(
                    "x264_param_default_preset failed: {ret}"
                )));
            }

            // Set parameters via x264_param_parse (ABI-stable string key-value API)
            let parse = |key: &str, value: &str| -> EncoderResult<()> {
                let c_key = CString::new(key).unwrap();
                let c_val = CString::new(value).unwrap();
                let ret = (self.api.param_parse)(param, c_key.as_ptr(), c_val.as_ptr());
                if ret != 0 {
                    return Err(EncoderError::InitFailed(format!(
                        "x264_param_parse failed: {key}={value} (code {ret})"
                    )));
                }
                Ok(())
            };

            // Core encoding parameters for real-time screen capture
            parse("width", &width.to_string())?;
            parse("height", &height.to_string())?;
            parse("threads", &self.threads_str())?;
            // CRF (constant quality) mode — better for screen content than CBR
            let crf = self.crf_value();
            parse("crf", &crf.to_string())?;
            // QP range limits
            parse("qpmin", &self.config.qp_min.to_string())?;
            parse("qpmax", &self.config.qp_max.to_string())?;
            // Frame rate
            parse("fps", &self.config.max_fps.to_string())?;
            // Keyframe interval: large value (we manage IDR manually via force_idr)
            parse("keyint", "1000")?;
            parse("min-keyint", "1000")?;
            // Disable scene-cut detection (we manage IDR timing ourselves)
            parse("scenecut", "0")?;
            // No B-frames (zerolatency tune already sets this, but be explicit)
            parse("bframes", "0")?;
            // Input colorspace: I420 (YUV 4:2:0 planar)
            // We convert BGRA→I420 in Rust because x264's internal BGRA
            // conversion produces 4:4:4 profile which mstsc can't decode.
            parse("input-csp", "i420")?;
            // Output: Annex B format (start codes, not AVCC)
            parse("annexb", "1")?;
            // Log level: errors only
            parse("log-level", "error")?;
            // No VBV (no buffering constraints for real-time)
            parse("vbv-maxrate", "0")?;
            parse("vbv-bufsize", "0")?;
            // Sliced threads for parallelism on multi-core
            if self.config.encoder_threads > 1 || self.config.encoder_threads == 0 {
                parse("sliced-threads", "1")?;
            }
            // Intra refresh: disabled (we use explicit IDR via force_idr)
            parse("intra-refresh", "0")?;
            // Repeat headers: send SPS/PPS with every IDR (needed for RDP)
            parse("repeat-headers", "1")?;

            // Open encoder
            self.encoder = (self.api.encoder_open)(param);
            if self.encoder.is_null() {
                return Err(EncoderError::InitFailed(
                    "x264_encoder_open returned NULL".to_string(),
                ));
            }

            // Cleanup the param struct (frees any internal allocations)
            (self.api.param_cleanup)(param);

            self.width = width;
            self.height = height;
            self.needs_reinit = false;

            info!(
                "x264 encoder opened: {}×{}, crf={}, threads={}, preset=ultrafast, tune=zerolatency",
                width, height, crf, self.threads_str()
            );
        }
        Ok(())
    }

    /// Determine the number of threads string for x264.
    fn threads_str(&self) -> String {
        if self.config.encoder_threads == 0 {
            "0".to_string() // 0 = auto (x264 detects CPU count)
        } else {
            self.config.encoder_threads.to_string()
        }
    }

    /// Determine CRF value from config.
    ///
    /// CRF (Constant Rate Factor) is x264's quality-based rate control.
    /// Lower = higher quality. For screen content:
    /// - CRF 18: visually lossless
    /// - CRF 23: default (good quality)
    /// - CRF 28: lower quality but smaller
    ///
    /// We map from the config's QP range to a reasonable CRF.
    fn crf_value(&self) -> u8 {
        // Use qp_min as the CRF basis — it represents the best quality target.
        // For screen content, QP 1-10 (current config) maps to CRF 1-10.
        // Cap at 23 to avoid extreme quality at high CPU cost.
        self.config.qp_min.min(23)
    }

    /// Encode a BGRA frame to H.264 Annex B.
    pub fn encode_bgra(
        &mut self,
        bgra_data: &[u8],
        width: u32,
        height: u32,
        timestamp_ms: u64,
    ) -> EncoderResult<Option<H264Frame>> {
        // Validate dimensions
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(EncoderError::InvalidDimensions { width, height });
        }

        let expected_size = (width * height * 4) as usize;
        if bgra_data.len() < expected_size {
            return Err(EncoderError::EncodeFailed(format!(
                "BGRA buffer too small: {} < {}",
                bgra_data.len(),
                expected_size
            )));
        }

        // (Re)initialize encoder if dimensions changed or first frame
        if self.needs_reinit || self.width != width || self.height != height {
            self.reinit(width, height)?;
            // Allocate I420 plane buffers
            let y_size = (width * height) as usize;
            let uv_size = ((width / 2) * (height / 2)) as usize;
            self.y_plane = vec![0u8; y_size];
            self.u_plane = vec![0u8; uv_size];
            self.v_plane = vec![0u8; uv_size];
        }

        // BGRA → I420 (YUV 4:2:0 planar) conversion
        // When the `h264` feature is enabled, reuse OpenH264's optimized
        // YUVBuffer::from_rgb_source (same conversion the OpenH264 encoder uses).
        // Otherwise, fall back to a naive per-pixel BT.601 conversion.
        let w = width as usize;
        let h = height as usize;
        let half_w = w / 2;
        let half_h = h / 2;

        #[cfg(feature = "h264")]
        {
            let bgra_source = BgraSliceU8::new(bgra_data, (w, h));
            let yuv = YUVBuffer::from_rgb_source(bgra_source);
            let (y_stride, u_stride, v_stride) = yuv.strides();

            // Copy Y plane (stride may differ from width)
            if y_stride == w {
                self.y_plane.copy_from_slice(yuv.y());
            } else {
                for j in 0..h {
                    self.y_plane[j * w..(j + 1) * w]
                        .copy_from_slice(&yuv.y()[j * y_stride..j * y_stride + w]);
                }
            }
            // Copy U plane
            if u_stride == half_w {
                self.u_plane.copy_from_slice(yuv.u());
            } else {
                for j in 0..half_h {
                    self.u_plane[j * half_w..(j + 1) * half_w]
                        .copy_from_slice(&yuv.u()[j * u_stride..j * u_stride + half_w]);
                }
            }
            // Copy V plane
            if v_stride == half_w {
                self.v_plane.copy_from_slice(yuv.v());
            } else {
                for j in 0..half_h {
                    self.v_plane[j * half_w..(j + 1) * half_w]
                        .copy_from_slice(&yuv.v()[j * v_stride..j * v_stride + half_w]);
                }
            }
        }

        #[cfg(not(feature = "h264"))]
        {
            // Fallback: naive per-pixel BT.601 limited range conversion
            for j in 0..h {
                for i in 0..w {
                    let idx = (j * w + i) * 4;
                    let b = bgra_data[idx] as i32;
                    let g = bgra_data[idx + 1] as i32;
                    let r = bgra_data[idx + 2] as i32;
                    self.y_plane[j * w + i] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                }
            }
            for j in 0..half_h {
                for i in 0..half_w {
                    let idx = ((j * 2) * w + (i * 2)) * 4;
                    let b = bgra_data[idx] as i32;
                    let g = bgra_data[idx + 1] as i32;
                    let r = bgra_data[idx + 2] as i32;
                    self.u_plane[j * half_w + i] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
                    self.v_plane[j * half_w + i] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
                }
            }
        }

        unsafe {
            // Initialize the input picture using x264_picture_init
            self.pic_in_buf.fill(0);
            let pic = self.pic_in_buf.as_mut_ptr();

            (self.api.picture_init)(pic as *mut c_void);

            // Set picture fields by known offsets
            // i_type: force IDR or auto
            let is_forced_idr = self.force_idr;
            if self.force_idr {
                write_i32_at(pic, PIC_OFF_I_TYPE, X264_TYPE_IDR);
                self.force_idr = false;
            } else {
                write_i32_at(pic, PIC_OFF_I_TYPE, X264_TYPE_AUTO);
            }

            // i_pts
            write_i64_at(pic, PIC_OFF_I_PTS, timestamp_ms as i64);

            // param pointer: NULL (use encoder's current params)
            write_ptr_at(pic, PIC_OFF_PARAM, std::ptr::null());

            // img.i_csp: I420 (YUV 4:2:0 planar)
            write_i32_at(pic, IMG_OFF_I_CSP, X264_CSP_I420);
            // img.i_plane: 3 (Y, U, V)
            write_i32_at(pic, IMG_OFF_I_PLANE, 3);
            // img.i_stride[0]: width (Y plane stride)
            write_i32_at(pic, IMG_OFF_I_STRIDE, width as c_int);
            // img.i_stride[1]: width/2 (U plane stride)
            write_i32_at(pic, IMG_OFF_I_STRIDE + 4, (width / 2) as c_int);
            // img.i_stride[2]: width/2 (V plane stride)
            write_i32_at(pic, IMG_OFF_I_STRIDE + 8, (width / 2) as c_int);
            // img.i_stride[3]: 0
            write_i32_at(pic, IMG_OFF_I_STRIDE + 12, 0);
            // img.plane[0]: Y plane
            write_ptr_at(pic, IMG_OFF_PLANE, self.y_plane.as_ptr());
            // img.plane[1]: U plane
            write_ptr_at(pic, IMG_OFF_PLANE + 8, self.u_plane.as_ptr());
            // img.plane[2]: V plane
            write_ptr_at(pic, IMG_OFF_PLANE + 16, self.v_plane.as_ptr());
            // img.plane[3]: NULL
            write_ptr_at(pic, IMG_OFF_PLANE + 24, std::ptr::null());

            // Encode
            let mut nal_ptr: *mut u8 = std::ptr::null_mut();
            let mut i_nal: c_int = 0;

            let ret = (self.api.encoder_encode)(
                self.encoder,
                &mut nal_ptr,
                &mut i_nal,
                pic,
                std::ptr::null_mut(),
            );

            if ret < 0 {
                return Err(EncoderError::EncodeFailed(format!(
                    "x264_encoder_encode failed: {ret}"
                )));
            }

            if i_nal == 0 || nal_ptr.is_null() {
                // x264 can return 0 NALs if the frame was dropped (rare with zerolatency)
                return Ok(None);
            }

            // Concatenate all NAL units into a single Annex B bitstream.
            // x264 outputs NALs WITH start codes when b_annexb is set (which we
            // set via "annexb=1"). The p_payload points to start-code-prefixed data.
            let mut data = Vec::new();
            let mut is_keyframe = false;

            for i in 0..i_nal as usize {
                // Each NAL is X264_NAL_SIZE bytes; access fields by offset
                let nal = nal_ptr.add(i * X264_NAL_SIZE);

                // Read i_type (NAL type: 5 = IDR)
                let nal_type = read_i32_at(nal, NAL_OFF_I_TYPE);
                if nal_type == 5 {
                    is_keyframe = true;
                }

                // Read i_payload (size) and p_payload (data pointer)
                let payload_size = read_i32_at(nal, NAL_OFF_I_PAYLOAD) as usize;
                let payload_ptr = read_ptr_at(nal, NAL_OFF_P_PAYLOAD);

                if payload_size > 0 && !payload_ptr.is_null() {
                    let payload = std::slice::from_raw_parts(payload_ptr, payload_size);
                    data.extend_from_slice(payload);
                }
            }

            if is_forced_idr && !is_keyframe {
                // If we forced IDR but x264 didn't produce one, treat the frame as
                // keyframe anyway (the force flag may not always produce IDR if
                // the encoder is in a state where it can't)
                let first_nal_type = if i_nal > 0 {
                    read_i32_at(nal_ptr, NAL_OFF_I_TYPE)
                } else {
                    -1
                };
                debug!(
                    "x264: forced IDR but no IDR NAL found (type={}), treating as keyframe",
                    first_nal_type
                );
                is_keyframe = true;
            }

            if data.is_empty() {
                warn!("x264: encoded bitstream is empty");
                return Ok(None);
            }

            self.frame_count += 1;

            // Log NAL structure at debug level
            if tracing::enabled!(tracing::Level::DEBUG) {
                Self::log_nal_structure(&data, self.frame_count, is_keyframe);
            }

            // Diagnostics
            super::encode_diagnostics::log_nal_hex_dump(&data, self.frame_count, "x264-AVC420");
            if let Some(d) = &self.diagnostics {
                d.dump_frame(&data);
                d.self_test(&data, "x264-AVC420");
            }

            Ok(Some(H264Frame {
                size: data.len(),
                data,
                is_keyframe,
                timestamp_ms,
            }))
        }
    }

    /// Force the next encoded frame to be an IDR keyframe.
    pub fn force_keyframe(&mut self) {
        self.force_idr = true;
        debug!("x264: forced keyframe on next encode");
    }

    /// Get encoder statistics.
    pub fn stats(&self) -> super::encoder::EncoderStats {
        super::encoder::EncoderStats {
            frames_encoded: self.frame_count,
            bitrate_kbps: self.config.bitrate_kbps,
        }
    }

    fn log_nal_structure(data: &[u8], frame_num: u64, is_keyframe: bool) {
        let mut nal_types = Vec::new();
        let mut i = 0;

        while i < data.len() {
            let start_code_len = if i + 4 <= data.len() && data[i..i + 4] == [0x00, 0x00, 0x00, 0x01]
            {
                4
            } else if i + 3 <= data.len() && data[i..i + 3] == [0x00, 0x00, 0x01] {
                3
            } else {
                i += 1;
                continue;
            };

            let nal_start = i + start_code_len;
            if nal_start >= data.len() {
                break;
            }

            let nal_header = data[nal_start];
            let nal_type = nal_header & 0x1F;

            let mut nal_end = data.len();
            let mut j = nal_start + 1;
            while j + 2 < data.len() {
                if (data[j..j + 3] == [0x00, 0x00, 0x01])
                    || (j + 3 < data.len() && data[j..j + 4] == [0x00, 0x00, 0x00, 0x01])
                {
                    nal_end = j;
                    break;
                }
                j += 1;
            }

            let nal_size = nal_end - nal_start;
            let type_name = match nal_type {
                1 => "P-slice",
                2 => "B-slice",
                5 => "IDR",
                6 => "SEI",
                7 => "SPS",
                8 => "PPS",
                9 => "AU-delim",
                _ => "Other",
            };

            nal_types.push(format!("{type_name}({nal_size}b)"));
            i = nal_end;
            if i == data.len() {
                break;
            }
        }

        debug!(
            "📦 [x264] Frame {}: {} | NALs: [{}] | Total: {}b",
            frame_num,
            if is_keyframe { "IDR" } else { "P" },
            nal_types.join(", "),
            data.len()
        );
    }
}

impl Drop for X264Encoder {
    fn drop(&mut self) {
        unsafe {
            if !self.encoder.is_null() {
                (self.api.encoder_close)(self.encoder);
                self.encoder = std::ptr::null_mut();
            }
        }
    }
}

// Stub when x264 feature is disabled
#[cfg(not(feature = "x264"))]
pub struct X264Encoder;

#[cfg(not(feature = "x264"))]
impl X264Encoder {
    pub fn new(_config: EncoderConfig) -> EncoderResult<Self> {
        Err(EncoderError::FeatureDisabled)
    }

    pub fn set_diagnostics(
        &mut self,
        _diagnostics: Option<std::sync::Arc<super::encode_diagnostics::EncodeDiagnostics>>,
    ) {
    }

    pub fn encode_bgra(
        &mut self,
        _bgra_data: &[u8],
        _width: u32,
        _height: u32,
        _timestamp_ms: u64,
    ) -> EncoderResult<Option<H264Frame>> {
        Err(EncoderError::FeatureDisabled)
    }

    pub fn force_keyframe(&mut self) {}

    pub fn stats(&self) -> super::encoder::EncoderStats {
        super::encoder::EncoderStats {
            frames_encoded: 0,
            bitrate_kbps: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "x264")]
    #[test]
    fn test_x264_encoder_creation() {
        let config = EncoderConfig::default();
        let encoder = X264Encoder::new(config);
        match encoder {
            Ok(_) => {}
            Err(EncoderError::InitFailed(e)) => {
                eprintln!("x264 not available, skipping: {e}");
                return;
            }
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }

    #[cfg(feature = "x264")]
    #[test]
    fn test_x264_encode_small_frame() {
        let config = EncoderConfig::default();
        let mut encoder = match X264Encoder::new(config) {
            Ok(e) => e,
            Err(EncoderError::InitFailed(_)) => return,
            Err(e) => panic!("unexpected error: {e:?}"),
        };

        let width = 64u32;
        let height = 64u32;
        let bgra_data = vec![0u8; (width * height * 4) as usize];

        let result = encoder.encode_bgra(&bgra_data, width, height, 0);
        assert!(result.is_ok(), "encode failed: {:?}", result.err());
        let frame = result.unwrap().expect("should produce a frame");
        assert!(frame.is_keyframe, "first frame should be IDR");
        assert!(!frame.data.is_empty(), "encoded data should not be empty");
    }

    #[cfg(feature = "x264")]
    #[test]
    fn test_x264_encode_p_frame() {
        let config = EncoderConfig::default();
        let mut encoder = match X264Encoder::new(config) {
            Ok(e) => e,
            Err(EncoderError::InitFailed(_)) => return,
            Err(e) => panic!("unexpected error: {e:?}"),
        };

        let width = 128u32;
        let height = 128u32;
        let bgra_data = vec![0u8; (width * height * 4) as usize];

        // First frame: IDR
        let f1 = encoder
            .encode_bgra(&bgra_data, width, height, 0)
            .unwrap()
            .unwrap();
        assert!(f1.is_keyframe);

        // Second frame: P-frame (different content)
        let mut bgra2 = vec![0xFFu8; (width * height * 4) as usize];
        for i in 0..bgra2.len() {
            bgra2[i] = ((i * 7) % 256) as u8;
        }
        let f2 = encoder
            .encode_bgra(&bgra2, width, height, 16)
            .unwrap()
            .unwrap();
        assert!(!f2.is_keyframe, "second frame should not be IDR");
    }

    #[cfg(feature = "x264")]
    #[test]
    fn test_x264_force_keyframe() {
        let config = EncoderConfig::default();
        let mut encoder = match X264Encoder::new(config) {
            Ok(e) => e,
            Err(EncoderError::InitFailed(_)) => return,
            Err(e) => panic!("unexpected error: {e:?}"),
        };

        let width = 64u32;
        let height = 64u32;
        let bgra_data = vec![0u8; (width * height * 4) as usize];

        // First frame
        let _ = encoder.encode_bgra(&bgra_data, width, height, 0).unwrap();

        // Force keyframe
        encoder.force_keyframe();

        // Next frame should be IDR
        let frame = encoder
            .encode_bgra(&bgra_data, width, height, 16)
            .unwrap()
            .unwrap();
        assert!(frame.is_keyframe, "forced keyframe should produce IDR");
    }
}
