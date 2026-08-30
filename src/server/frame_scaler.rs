//! BGRA frame scaling for client-requested desktop sizes (resolution support).
//!
//! The RDP desktop size is now decoupled from the compositor capture size:
//! the client's dialog choice (via `with_honor_client_desktop_size`) or a
//! Display-Control resize may request a size the compositor cannot match
//! (hyperv_drm exposes a fixed 18-mode list, max 1920x1080/1600x1200). The
//! pipeline then serves the requested desktop by scaling captured frames.
//!
//! Mapping: capture compositor size -> RDP desktop size (this module, video
//! direction) and desktop -> capture (input direction, see
//! `input_handler::map_desktop_to_capture`).
//!
//! The scaler is region-aware: damage rectangles arrive in CAPTURE space and
//! are mapped into desktop space with rounding-out so no damaged pixel is
//! dropped at downscale boundaries.

/// Scale factors between capture (compositor) and RDP desktop spaces.
///
/// Stored as `(num, den)` integer ratios of desktop/capture per axis to keep
/// coordinate mapping exact and reversible:
/// `desktop = capture * num / den` and `capture = desktop * den / num`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScaleFactors {
    /// Desktop width / capture width as (numerator, denominator), reduced.
    pub x: (u32, u32),
    /// Desktop height / capture height as (numerator, denominator), reduced.
    pub y: (u32, u32),
}

impl ScaleFactors {
    /// Nothing to do: capture and desktop sizes match.
    pub fn is_identity(&self) -> bool {
        self.x == (1, 1) && self.y == (1, 1)
    }

    /// Compute factors for capture `(cw, ch)` -> desktop `(dw, dh)`.
    pub fn new(cw: u32, ch: u32, dw: u32, dh: u32) -> Self {
        Self {
            x: reduce(dw, cw),
            y: reduce(dh, ch),
        }
    }

    /// Map a capture-space coordinate into desktop space (floor — point math).
    pub fn map_x(&self, x: u32) -> u32 {
        div_ceil(x.saturating_mul(self.x.0), self.x.1.max(1))
    }

    /// Map a capture-space coordinate into desktop space (floor — point math).
    pub fn map_y(&self, y: u32) -> u32 {
        div_ceil(y.saturating_mul(self.y.0), self.y.1.max(1))
    }

    /// Map a desktop-space coordinate back into capture space (input path).
    pub fn unmap_x(&self, x: u32) -> u32 {
        div_ceil(x.saturating_mul(self.x.1), self.x.0.max(1))
    }

    /// Map a desktop-space coordinate back into capture space (input path).
    pub fn unmap_y(&self, y: u32) -> u32 {
        div_ceil(y.saturating_mul(self.y.1), self.y.0.max(1))
    }

    /// Map a capture-space rectangle into desktop space, rounding OUT so the
    /// destination region fully covers every scaled source pixel.
    pub fn map_rect_out(&self, r: (u32, u32, u32, u32)) -> (u32, u32, u32, u32) {
        let x0 = r.0.saturating_mul(self.x.0) / self.x.1.max(1);
        let y0 = r.1.saturating_mul(self.y.0) / self.y.1.max(1);
        let x1 = div_ceil(r.2.saturating_mul(self.x.0), self.x.1.max(1));
        let y1 = div_ceil(r.3.saturating_mul(self.y.0), self.y.1.max(1));
        (x0, y0, x1, y1)
    }
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

fn reduce(num: u32, den: u32) -> (u32, u32) {
    // Guard the degenerate cases: zero dimensions fall back to identity so
    // the pipeline never divides by zero (callers clamp sizes to >= 1 first).
    let (num, den) = (num.max(1), den.max(1));
    let g = gcd(num, den);
    (num / g, den / g)
}

fn div_ceil(a: u32, b: u32) -> u32 {
    if b == 0 {
        return a;
    }
    (a + b - 1) / b
}

/// Nearest-neighbor BGRA frame scale.
///
/// Chosen deliberately over bilinear: RDP desktop content is UI text and
/// chrome; at near-1.0 and moderate factors NN keeps edges crisp where
/// bilinear smears, and it is allocation-free friendly + trivially SIMDable
/// later. Input is tightly packed BGRA (4 bytes/px), `src` row stride =
/// `sw*4`.
///
/// Returns a new tightly packed buffer at `(dw, dh)`, or the input slice
/// wrapped in a Vec when sizes already match (identity fast path).
pub fn scale_bgra(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    debug_assert_eq!(src.len() % 4, 0, "BGRA input");
    if sw == dw && sh == dh || sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return src.to_vec();
    }

    let mut dst = vec![0u8; (dw as usize) * (dh as usize) * 4];
    // Fixed-point horizontal sampling table: for each destination column,
    // the source column that feeds it (x * sw / dw).
    let mut x_src = Vec::with_capacity(dw as usize);
    for dx in 0..dw {
        x_src.push(((dx as u64 * sw as u64) / dw.max(1) as u64) as u32);
    }

    for dy in 0..dh {
        let sy = ((dy as u64 * sh as u64) / dh.max(1) as u64) as u32;
        let src_row = &src[(sy as usize * sw as usize * 4)..][..(sw as usize * 4)];
        let dst_row = &mut dst[(dy as usize * dw as usize * 4)..][..(dw as usize * 4)];
        for (dx, &sx) in x_src.iter().enumerate() {
            let s = &src_row[(sx as usize * 4)..][..4];
            dst_row[dx * 4..dx * 4 + 4].copy_from_slice(s);
        }
    }
    dst
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_factors() {
        let f = ScaleFactors::new(1920, 1080, 1920, 1080);
        assert!(f.is_identity());
        assert_eq!(f.map_x(123), 123);
        assert_eq!(f.unmap_y(456), 456);
    }

    #[test]
    fn upscaling_1080p_to_1440p_math() {
        // 2560/1920 = 4/3, 1440/1080 = 4/3
        let f = ScaleFactors::new(1920, 1080, 2560, 1440);
        assert_eq!(f.x, (4, 3));
        assert_eq!(f.y, (4, 3));
        assert!(!f.is_identity());
        assert_eq!(f.map_x(1920), 2560);
        assert_eq!(f.map_y(1080), 1440);
        // Reversible corner
        assert_eq!(f.unmap_x(2560), 1920);
        assert_eq!(f.unmap_y(1440), 1080);
    }

    #[test]
    fn downscale_capture_to_5_4_desktop() {
        // 1280x1024 desktop from 1920x1080 capture
        let f = ScaleFactors::new(1920, 1080, 1280, 1024);
        assert_eq!(f.map_x(1920), 1280);
        assert_eq!(f.map_y(1080), 1024);
        // Rect round-out: full capture maps to full desktop
        let r = f.map_rect_out((0, 0, 1920, 1080));
        assert_eq!(r, (0, 0, 1280, 1024));
    }

    #[test]
    fn scale_bgra_identity_returns_copy() {
        let src = vec![1u8, 2, 3, 4];
        let out = scale_bgra(&src, 1, 1, 1, 1);
        assert_eq!(out, src);
    }

    #[test]
    fn scale_bgra_double_width() {
        // 2x1 -> 4x1: each source pixel duplicated horizontally
        let src: Vec<u8> = vec![10, 20, 30, 255, 40, 50, 60, 255];
        let out = scale_bgra(&src, 2, 1, 4, 1);
        assert_eq!(out.len(), 16);
        assert_eq!(&out[0..4], &[10, 20, 30, 255]);
        assert_eq!(&out[4..8], &[10, 20, 30, 255]);
        assert_eq!(&out[8..12], &[40, 50, 60, 255]);
        assert_eq!(&out[12..16], &[40, 50, 60, 255]);
    }

    #[test]
    fn scale_bgra_corners_survive() {
        // 2x2 -> 3x3: corner pixels must be reachable (containment)
        let src: Vec<u8> = (0..16).collect(); // 4 px
        let out = scale_bgra(&src, 2, 2, 3, 3);
        assert_eq!(out.len(), 36);
        // top-left maps to top-left
        assert_eq!(&out[0..4], &src[0..4]);
        // bottom-right maps to bottom-right
        assert_eq!(&out[32..36], &src[12..16]);
    }

    #[test]
    fn degenerate_sizes_stay_safe() {
        let f = ScaleFactors::new(0, 1080, 1920, 1080);
        // Guards kick in: no panic; mapping grounded
        let _ = f.map_x(10);
        let out = scale_bgra(&[0u8; 4], 1, 1, 0, 0);
        assert_eq!(out, &[0u8; 4]);
    }
}
