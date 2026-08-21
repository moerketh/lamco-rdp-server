//! Zero-allocation integer BT.601 BGRA → I420 conversion.
//!
//! Coefficients match the openh264 crate's fast scalar path
//! (`formats::rgb2yuv::write_yuv_scalar`) so output is bit-comparable
//! with the previous `YUVBuffer::from_rgb_source` pipeline, but:
//! - no per-frame allocation (writes into a caller-provided buffer)
//! - integer-only arithmetic (no per-pixel f32 rounds like `read_rgb`)
//! - writes the three planes contiguously (Y, then U, then V), which is
//!   exactly the layout the x264 C shim consumes — no plane copies.
//!
//! Two range encodings are supported (see `Range`): the default
//! limited-range matches OpenH264 bit-for-bit; full-range maps black to
//! Y0/white to Y255 and is used when the client (mstsc) does not perform
//! limited→full expansion — grey-blacks fix, 2026-08-21.

/// YUV value range the converter emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Range {
    /// Video/limited range: Y 16–235, UV 16–240 (studio swing). OpenH264-identical.
    #[default]
    Limited,
    /// Full/PC range: Y 0–255, UV 0–255 (JPEG-style). Black ⇒ Y0, not Y16.
    Full,
}

/// Converts a packed BGRA frame into a contiguous I420 buffer.
///
/// `yuv` must be `width * height * 3 / 2` bytes: Y plane (`w*h`) followed by
/// U (`w/2 * h/2`) followed by V (`w/2 * h/2`).
///
/// # Panics
/// Panics if `width`/`height` are not multiples of 2 or buffers are undersized.
pub fn convert(bgra: &[u8], width: usize, height: usize, yuv: &mut [u8], range: Range) {
    assert!(width % 2 == 0, "width must be a multiple of 2");
    assert!(height % 2 == 0, "height must be a multiple of 2");
    let y_size = width * height;
    let uv_size = (width / 2) * (height / 2);
    assert!(
        yuv.len() >= y_size + 2 * uv_size,
        "yuv buffer too small: {} < {}",
        yuv.len(),
        y_size + 2 * uv_size
    );
    assert!(
        bgra.len() >= y_size * 4,
        "bgra buffer too small: {} < {}",
        bgra.len(),
        y_size * 4
    );

    let half_width = width / 2;
    let (y_plane, uv_planes) = yuv.split_at_mut(y_size);
    let (u_plane, v_plane) = uv_planes.split_at_mut(uv_size);

    match range {
        Range::Limited => {
            // Luma: integer BT.601, identical result to openh264's
            // write_yuv_scalar. BGRA layout: [B, G, R, A] per pixel.
            for (src, y) in bgra.chunks_exact(4).zip(y_plane.iter_mut()) {
                *y = (((66 * u32::from(src[2])            // R
                    + 129 * u32::from(src[1])             // G
                    + 25 * u32::from(src[0]))             // B
                    >> 8)
                    + 16) as u8;
            }

            // Chroma: 2×2 box average in RGB space, then integer BT.601,
            // matching openh264's write_yuv_scalar chroma path (rounding
            // via +2 / 4).
            for row in 0..height / 2 {
                let r0 = &bgra[row * 2 * width * 4..];
                let r1 = &bgra[(row * 2 + 1) * width * 4..];
                let u_row = &mut u_plane[row * half_width..(row + 1) * half_width];
                let v_row = &mut v_plane[row * half_width..(row + 1) * half_width];
                for col in 0..half_width {
                    let p00 = &r0[col * 8..col * 8 + 4];
                    let p01 = &r0[col * 8 + 4..col * 8 + 8];
                    let p10 = &r1[col * 8..col * 8 + 4];
                    let p11 = &r1[col * 8 + 4..col * 8 + 8];
                    let b = (i16::from(p00[0]) + i16::from(p01[0]) + i16::from(p10[0]) + i16::from(p11[0]) + 2) / 4;
                    let g = (i16::from(p00[1]) + i16::from(p01[1]) + i16::from(p10[1]) + i16::from(p11[1]) + 2) / 4;
                    let r = (i16::from(p00[2]) + i16::from(p01[2]) + i16::from(p10[2]) + i16::from(p11[2]) + 2) / 4;
                    u_row[col] = (((-38 * r + 112 * b - 74 * g) >> 8) + 128) as u8;
                    v_row[col] = (((112 * r - 18 * b - 94 * g) >> 8) + 128) as u8;
                }
            }
        }
        Range::Full => {
            // Full-range (JPEG-style) BT.601: luma scaled 0–255 with
            // rounding (+128 before >>8), chroma deltas scaled by 256/224
            // centered on 128 so grayscale stays perfectly neutral (UV=128).
            // Constants: 299/1000, 587/1000, 114/1000 (×256 ⇒ 76.5, 150.2,
            // 29.2, rounded) and 512/448 ≈ 1.143 (×256 ⇒ 292.6 ≈ 293).
            for (src, y) in bgra.chunks_exact(4).zip(y_plane.iter_mut()) {
                *y = ((77 * u32::from(src[2])            // R
                    + 150 * u32::from(src[1])            // G
                    + 29 * u32::from(src[0])             // B
                    + 128)
                    >> 8) as u8;
            }

            for row in 0..height / 2 {
                let r0 = &bgra[row * 2 * width * 4..];
                let r1 = &bgra[(row * 2 + 1) * width * 4..];
                let u_row = &mut u_plane[row * half_width..(row + 1) * half_width];
                let v_row = &mut v_plane[row * half_width..(row + 1) * half_width];
                for col in 0..half_width {
                    let p00 = &r0[col * 8..col * 8 + 4];
                    let p01 = &r0[col * 8 + 4..col * 8 + 8];
                    let p10 = &r1[col * 8..col * 8 + 4];
                    let p11 = &r1[col * 8 + 4..col * 8 + 8];
                    let b = (i16::from(p00[0]) + i16::from(p01[0]) + i16::from(p10[0]) + i16::from(p11[0]) + 2) / 4;
                    let g = (i16::from(p00[1]) + i16::from(p01[1]) + i16::from(p10[1]) + i16::from(p11[1]) + 2) / 4;
                    let r = (i16::from(p00[2]) + i16::from(p01[2]) + i16::from(p10[2]) + i16::from(p11[2]) + 2) / 4;
                    // Full-range chroma (JPEG/BT.601 full):
                    //   Cb = 128 + (−0.168736R − 0.331264G + 0.5B)
                    //   Cr = 128 + ( 0.5R − 0.418688G − 0.081312B)
                    // Fixed point ×256 with rounding (+128) and center
                    // 128<<8 = 32768. Grayscale (r=g=b) cancels exactly ⇒ 128.
                    // Clamp: pure blue/red push Cb/Cr past 255.
                    let cb: i32 = 32768 + 128 - 43 * r as i32 - 85 * g as i32 + 128 * b as i32;
                    let cr: i32 = 32768 + 128 + 128 * r as i32 - 107 * g as i32 - 21 * b as i32;
                    u_row[col] = (cb >> 8).clamp(0, 255) as u8;
                    v_row[col] = (cr >> 8).clamp(0, 255) as u8;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "h264")]
    #[test]
    fn test_matches_openh264_from_rgb_source() {
        use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};

        let width = 64usize;
        let height = 64usize;
        // Deterministic pseudo-random BGRA pattern.
        let mut bgra = vec![0u8; width * height * 4];
        let mut x: u32 = 0x12345678;
        for px in bgra.chunks_exact_mut(4) {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            px[0] = (x & 0xFF) as u8;
            px[1] = ((x >> 8) & 0xFF) as u8;
            px[2] = ((x >> 16) & 0xFF) as u8;
            px[3] = 255;
        }

        let reference = YUVBuffer::from_rgb_source(BgraSliceU8::new(&bgra, (width, height)));
        let mut ours = vec![0u8; width * height * 3 / 2];
        convert(&bgra, width, height, &mut ours, Range::Limited);

        // from_rgb_source uses per-pixel f32 averaging; we use integer
        // averaging. Allow ±1 LSB difference (same tolerance the openh264
        // crate uses when asserting its two paths match).
        let (ty, tu, tv) = (
            reference.y(),
            reference.u(),
            reference.v(),
        );
        let y_size = width * height;
        let uv_size = (width / 2) * (height / 2);
        let (my, rest) = ours.split_at(y_size);
        let (mu, mv) = rest.split_at(uv_size);
        for (a, b) in my.iter().zip(ty.iter()) {
            assert!(a.abs_diff(*b) <= 1, "Y mismatch: {a} vs {b}");
        }
        for (a, b) in mu.iter().zip(tu.iter()) {
            assert!(a.abs_diff(*b) <= 1, "U mismatch: {a} vs {b}");
        }
        for (a, b) in mv.iter().zip(tv.iter()) {
            assert!(a.abs_diff(*b) <= 1, "V mismatch: {a} vs {b}");
        }
    }

    #[test]
    fn test_black_and_white_corners() {
        // Pure black must map to Y=16 U=128 V=128; pure white to Y=235.
        let width = 4usize;
        let height = 4usize;
        let mut bgra = vec![0u8; width * height * 4];
        for (i, px) in bgra.chunks_exact_mut(4).enumerate() {
            let white = i < width * height / 2;
            px[0] = u8::from(white) * 255;
            px[1] = u8::from(white) * 255;
            px[2] = u8::from(white) * 255;
            px[3] = 255;
        }
        let mut yuv = vec![0u8; width * height * 3 / 2];
        convert(&bgra, width, height, &mut yuv, Range::Limited);
        let y = &yuv[..width * height];
        assert!(y[..8].iter().all(|&v| v == 235), "white half Y=235");
        assert!(y[8..].iter().all(|&v| v == 16), "black half Y=16");
        let mid_uv = &yuv[width * height..];
        assert!(
            mid_uv.iter().all(|&v| (120..=136).contains(&v)),
            "grayscale chroma ≈ 128, got {mid_uv:?}"
        );
    }
    #[test]
    fn test_full_range_black_and_white_corners() {
        // Full-range: pure black ⇒ Y 0 (the grey-blacks fix), white ⇒ Y 255,
        // grayscale chroma exactly neutral (U=V=128).
        let width = 4usize;
        let height = 4usize;
        let mut bgra = vec![0u8; width * height * 4];
        for (i, px) in bgra.chunks_exact_mut(4).enumerate() {
            let white = i < width * height / 2;
            px[0] = u8::from(white) * 255;
            px[1] = u8::from(white) * 255;
            px[2] = u8::from(white) * 255;
            px[3] = 255;
        }
        let mut yuv = vec![0u8; width * height * 3 / 2];
        convert(&bgra, width, height, &mut yuv, Range::Full);
        let y = &yuv[..width * height];
        assert!(y[..8].iter().all(|&v| v == 255), "white half Y=255, got {:?}", &y[..8]);
        assert!(y[8..].iter().all(|&v| v == 0), "black half Y=0 (grey-blacks fix), got {:?}", &y[8..]);
        let mid_uv = &yuv[width * height..];
        assert!(
            mid_uv.iter().all(|&v| (126..=130).contains(&v)),
            "grayscale chroma = 128, got {mid_uv:?}"
        );
    }

    #[test]
    fn test_full_range_mid_gray_linearity() {
        // Sanity: mid-gray (128) should stay near the middle of the full
        // range, not drift toward either end.
        let width = 4usize;
        let height = 4usize;
        let bgra = vec![128u8; width * height * 4];
        let mut yuv = vec![0u8; width * height * 3 / 2];
        convert(&bgra, width, height, &mut yuv, Range::Full);
        let y = &yuv[..width * height];
        let mid = y[0];
        assert!((126..=130).contains(&mid), "mid-gray Y near 128, got {mid}");
        assert!(y.iter().all(|&v| v == mid), "uniform input ⇒ uniform luma");
    }}
