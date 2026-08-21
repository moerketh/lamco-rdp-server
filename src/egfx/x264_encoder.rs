//! x264 AVC420 encoder backed by a header-compiled C shim.
//!
//! The shim owns all x264 ABI structs. Rust performs the existing optimized
//! BGRA -> I420 conversion and passes three planes to x264 without copying
//! pixels again.

#![expect(unsafe_code, reason = "FFI calls to the x264 C shim")]

#[cfg(feature = "h264")]
use openh264::formats::BgraSliceU8;
#[cfg(test)]
use openh264::formats::{YUVBuffer, YUVSource};
use std::os::raw::{c_int, c_void};
use tracing::{debug, info};
use super::encoder::{EncoderConfig, EncoderError, EncoderResult, H264Frame};

const X264_TYPE_AUTO: c_int = 0;
const X264_TYPE_IDR: c_int = 1;
const X264_CSP_I420: c_int = 0x0002;

#[repr(C)]
struct X264Image {
    i_csp: c_int,
    i_plane: c_int,
    i_stride: [c_int; 4],
    plane: [*mut u8; 4],
}

#[repr(C, align(16))]
struct X264Picture {
    i_type: c_int,
    i_qpplus1: c_int,
    i_pic_struct: c_int,
    b_keyframe: c_int,
    i_pts: i64,
    i_dts: i64,
    param: *mut c_void,
    img: X264Image,
    rest: [u8; 144],
}

impl X264Picture {
    fn zeroed() -> Self {
        Self {
            i_type: X264_TYPE_AUTO,
            i_qpplus1: 0,
            i_pic_struct: 0,
            b_keyframe: 0,
            i_pts: 0,
            i_dts: 0,
            param: std::ptr::null_mut(),
            img: X264Image {
                i_csp: 0,
                i_plane: 0,
                i_stride: [0; 4],
                plane: [std::ptr::null_mut(); 4],
            },
            rest: [0; 144],
        }
    }
}

#[cfg(feature = "x264")]
unsafe extern "C" {
    fn lamco_x264_create(
        width: u32,
        height: u32,
        fps: u32,
        qp_min: u32,
        qp_max: u32,
        threads: u32,
        fullrange: u32,
    ) -> *mut c_void;
    fn lamco_x264_encode(
        encoder: *mut c_void,
        y: *const u8,
        u: *const u8,
        v: *const u8,
        y_stride: c_int,
        uv_stride: c_int,
        width: c_int,
        height: c_int,
        pts: i64,
        force_idr: c_int,
        output: *mut *mut u8,
        output_size: *mut c_int,
        is_keyframe: *mut c_int,
    ) -> c_int;
    fn lamco_x264_free(data: *mut u8);
    fn lamco_x264_destroy(encoder: *mut c_void);
}

/// H.264 encoder using x264 for AVC420.
///
/// Thread handling is CPU-count agnostic: `encoder_threads = 0` (the default)
/// is forwarded to x264 as `i_threads = 0` (X264_THREADS_AUTO), which sizes
/// the thread pool after the machine the server runs on.
pub struct X264Encoder {
    #[cfg(feature = "x264")]
    encoder: *mut c_void,
    config: EncoderConfig,
    frame_count: u64,
    width: u32,
    height: u32,
    force_idr: bool,
    /// Reusable contiguous Y/U/V buffer (Y then U then V, densely packed).
    /// Restored on resolution change; never re-allocated per frame.
    #[cfg(any(feature = "x264", feature = "h264"))]
    yuv: Vec<u8>,
    diagnostics: Option<std::sync::Arc<super::encode_diagnostics::EncodeDiagnostics>>,
}

#[cfg(feature = "x264")]
unsafe impl Send for X264Encoder {}

impl X264Encoder {
    pub fn new(config: EncoderConfig) -> EncoderResult<Self> {
        Ok(Self {
            #[cfg(feature = "x264")]
            encoder: std::ptr::null_mut(),
            config,
            frame_count: 0,
            width: 0,
            height: 0,
            force_idr: true,
            #[cfg(any(feature = "x264", feature = "h264"))]
            yuv: Vec::new(),
            diagnostics: None,
        })
    }

    pub fn set_diagnostics(
        &mut self,
        diagnostics: Option<std::sync::Arc<super::encode_diagnostics::EncodeDiagnostics>>,
    ) {
        self.diagnostics = diagnostics;
    }

    #[cfg(feature = "x264")]
    fn ensure_encoder(&mut self, width: u32, height: u32) -> EncoderResult<()> {
        if !self.encoder.is_null() && self.width == width && self.height == height {
            return Ok(());
        }
        if !self.encoder.is_null() {
            unsafe { lamco_x264_destroy(self.encoder) };
            self.encoder = std::ptr::null_mut();
        }
        let encoder = unsafe {
            lamco_x264_create(
                width,
                height,
                self.config.max_fps.max(1.0) as u32,
                self.config.qp_min as u32,
                self.config.qp_max as u32,
                self.config.encoder_threads as u32,
                self.fullrange() as u32,
            )
        };
        if encoder.is_null() {
            return Err(EncoderError::InitFailed(
                "x264 C shim could not initialize libx264".to_string(),
            ));
        }
        self.encoder = encoder;
        self.width = width;
        self.height = height;
        let range_desc = if self.fullrange() { "full" } else { "limited" };
        info!(
            "x264 encoder opened: {width}x{height}, I420, ultrafast/zerolatency, range={range_desc}"
        );
        Ok(())
    }

    /// Whether the encoder runs in full-range (Y 0-255) mode.
    ///
    /// Derived from `EncoderConfig.color_space` (populated from the
    /// `egfx.color_range` config knob). Default Limited to stay bit-identical
    /// with the OpenH264 path.
    fn fullrange(&self) -> bool {
        self.config
            .color_space
            .is_some_and(|cs| cs.range == super::color_space::ColorRange::Full)
    }

    #[cfg(feature = "x264")]
    pub fn encode_bgra(
        &mut self,
        bgra_data: &[u8],
        width: u32,
        height: u32,
        timestamp_ms: u64,
    ) -> EncoderResult<Option<H264Frame>> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(EncoderError::InvalidDimensions { width, height });
        }
        let expected = (width * height * 4) as usize;
        if bgra_data.len() < expected {
            return Err(EncoderError::EncodeFailed(format!(
                "BGRA buffer too small: {} < {}", bgra_data.len(), expected
            )));
        }
        self.ensure_encoder(width, height)?;
        let y_size = (width * height) as usize;
        let uv_size = ((width / 2) * (height / 2)) as usize;
        self.yuv.resize(y_size + 2 * uv_size, 0);

        // Zero-allocation integer BT.601 BGRA -> I420 conversion writing
        // straight into the contiguous Y/U/V buffer. Coefficients match the
        // openh264 crate's fast scalar path (write_yuv_scalar) in Limited
        // mode; Full mode maps black to Y0 for clients (mstsc) that do not
        // expand limited range.
        let convert_start = std::time::Instant::now();
        // Range decision must precede the mutable borrow of self.yuv.
        let range = if self.fullrange() {
            super::bgra_to_i420::Range::Full
        } else {
            super::bgra_to_i420::Range::Limited
        };
        #[cfg(feature = "h264")]
        super::bgra_to_i420::convert(
            bgra_data,
            width as usize,
            height as usize,
            &mut self.yuv,
            range,
        );
        #[cfg(not(feature = "h264"))]
        {
            let _ = &mut self.yuv;
            let _ = bgra_data;
            return Err(EncoderError::FeatureDisabled);
        }
        let convert_elapsed = convert_start.elapsed();

        let force_idr = self.force_idr;
        let mut output = std::ptr::null_mut();
        let mut output_size = 0;
        let mut is_keyframe = 0;
        let y_ptr = self.yuv.as_ptr();
        let u_ptr = unsafe { self.yuv.as_ptr().add(y_size) };
        let v_ptr = unsafe { self.yuv.as_ptr().add(y_size + uv_size) };
        let encode_start = std::time::Instant::now();
        let result = unsafe {
            lamco_x264_encode(
                self.encoder,
                y_ptr,
                u_ptr,
                v_ptr,
                width as c_int,
                (width / 2) as c_int,
                width as c_int,
                height as c_int,
                timestamp_ms as i64,
                i32::from(force_idr),
                &mut output,
                &mut output_size,
                &mut is_keyframe,
            )
        };
        let encode_elapsed = encode_start.elapsed();
        self.force_idr = false;
        debug!(
            convert_us = convert_elapsed.as_micros() as u64,
            encode_us = encode_elapsed.as_micros() as u64,
            "x264 stage timing"
        );
        if result <= 0 || output.is_null() || output_size <= 0 {
            return Ok(None);
        }
        let data = unsafe {
            let bytes = std::slice::from_raw_parts(output, output_size as usize).to_vec();
            lamco_x264_free(output);
            bytes
        };
        self.frame_count += 1;
        if let Some(diagnostics) = &self.diagnostics {
            diagnostics.dump_frame(&data);
            diagnostics.self_test(&data, "x264-AVC420");
        }
        Ok(Some(H264Frame {
            size: data.len(),
            data,
            is_keyframe: is_keyframe != 0,
            timestamp_ms,
        }))
    }

    #[cfg(not(feature = "x264"))]
    pub fn encode_bgra(
        &mut self,
        _bgra_data: &[u8],
        _width: u32,
        _height: u32,
        _timestamp_ms: u64,
    ) -> EncoderResult<Option<H264Frame>> {
        Err(EncoderError::FeatureDisabled)
    }

    pub fn force_keyframe(&mut self) {
        self.force_idr = true;
        debug!("x264: forced keyframe on next encode");
    }

    pub fn stats(&self) -> super::encoder::EncoderStats {
        super::encoder::EncoderStats {
            frames_encoded: self.frame_count,
            bitrate_kbps: self.config.bitrate_kbps,
        }
    }
}

#[cfg(feature = "x264")]
impl Drop for X264Encoder {
    fn drop(&mut self) {
        if !self.encoder.is_null() {
            unsafe { lamco_x264_destroy(self.encoder) };
            self.encoder = std::ptr::null_mut();
        }
    }
}

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
        _data: &[u8],
        _width: u32,
        _height: u32,
        _timestamp_ms: u64,
    ) -> EncoderResult<Option<H264Frame>> {
        Err(EncoderError::FeatureDisabled)
    }
    pub fn force_keyframe(&mut self) {}
    pub fn stats(&self) -> super::encoder::EncoderStats {
        super::encoder::EncoderStats { frames_encoded: 0, bitrate_kbps: 0 }
    }
}

#[cfg(feature = "h264")]
fn copy_plane(destination: &mut [u8], source: &[u8], source_stride: usize, width: usize, height: usize) {
    for row in 0..height {
        let source_start = row * source_stride;
        let destination_start = row * width;
        destination[destination_start..destination_start + width]
            .copy_from_slice(&source[source_start..source_start + width]);
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_x264_avc420_abi_constants() {
        assert_eq!(X264_TYPE_IDR, 1);
        assert_eq!(X264_CSP_I420, 0x0002);
        assert_eq!(std::mem::size_of::<X264Image>(), 56);
        assert_eq!(std::mem::size_of::<X264Picture>(), 240);
    }

    #[test]
    fn test_x264_picture_defaults_are_zeroed() {
        let picture = X264Picture::zeroed();
        assert_eq!(picture.i_type, X264_TYPE_AUTO);
        assert_eq!(picture.i_pts, 0);
        assert_eq!(picture.img.i_csp, 0);
        assert_eq!(picture.img.i_plane, 0);
        assert!(picture.img.plane.iter().all(|plane| plane.is_null()));
    }

    #[cfg(feature = "x264")]
    #[test]
    fn test_x264_emits_avc420_annex_b() {
        let mut encoder = X264Encoder::new(EncoderConfig::default()).unwrap();
        let frame = encoder
            .encode_bgra(&vec![0x40; 64 * 64 * 4], 64, 64, 0)
            .unwrap()
            .expect("x264 should emit the first frame");
        assert!(frame.is_keyframe);
        assert!(frame.data.starts_with(&[0, 0, 0, 1]));

        let sps_start = frame
            .data
            .windows(5)
            .position(|window| window == [0, 0, 0, 1, 0x67])
            .expect("IDR output must include an SPS NAL");
        // profile_idc must be a 4:2:0 profile decodable by mstsc's AVC420
        // decoder: 66=Baseline, 77=Main, 88=Extended, 100=High.
        // 244 (High 4:4:4 Predictive) is what caused the black screen.
        let profile_idc = frame.data[sps_start + 5];
        assert!(
            matches!(profile_idc, 66 | 77 | 88 | 100),
            "SPS profile_idc {profile_idc} is not 4:2:0-compatible"
        );
    }
}
