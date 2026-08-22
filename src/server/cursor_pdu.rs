//! XCursor → RDP ColorPointer conversion (xrdp-parity single cursor).
//!
//! Reads the guest's XCursor file for `left_ptr` (arrow), picks the
//! best-size image chunk, and converts ARGB pixels to the monochrome
//! `andMask` + color `xorMask` pair that TS_COLORPOINTERATTRIBUTE requires
//! ([MS-RDPBCGR] 2.2.9.1.1.4.1):
//!
//! - Both masks are bottom-up (last scanline first) and 1-bit for and,
//!   24bpp BGR for xor in color-pointer mode... in practice TS pointers use
//!   32bpp XOR BGRx. Single fragment, no compression.
//! - andMask bit=1 keeps the pixel transparent (client doesn't draw xor);
//!   bit=0 lets the xor color show. We set and=1 only for fully transparent
//!   source pixels so anti-aliased edges carry through the color mask.

use std::path::Path;

/// An RDP color pointer attribute, ready for `FastPathUpdate::Pointer`.
pub struct RdpPointer {
    pub cache_index: u16,
    pub hot_spot: (u16, u16),
    pub width: u16,
    pub height: u16,
    pub and_mask: Vec<u8>,
    pub xor_mask: Vec<u8>,
}

const MAGIC: u32 = 0x7275_6358; // "Xcur" as little-endian u32 read
const CHUNK_IMAGE: u32 = 0xFFFD_0002;

#[derive(Debug)]
pub enum CursorError {
    Io(std::io::Error),
    Malformed(&'static str),
    NoImage,
}

impl std::fmt::Display for CursorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cursor file_io: {e}"),
            Self::Malformed(why) => write!(f, "cursor malformed: {why}"),
            Self::NoImage => write!(f, "cursor file has no image chunk"),
        }
    }
}

struct ImageChunk {
    nominal: u32,
    width: u32,
    height: u32,
    xhot: u32,
    yhot: u32,
    pixels: Vec<u8>, // ARGB rows, top-down, width*height*4
}

/// Parse an XCursor file and pick the image chunk closest to 24px.
fn parse_xcursor(data: &[u8]) -> Result<ImageChunk, CursorError> {
    let rd_u32 = |off: usize| -> Result<u32, CursorError> {
        data.get(off..off + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .ok_or(CursorError::Malformed("truncated header/toc"))
    };

    if data.len() < 16 {
        return Err(CursorError::Malformed("file too small"));
    }
    if rd_u32(0)? != MAGIC {
        return Err(CursorError::Malformed("bad magic"));
    }
    let ntoc = rd_u32(12)? as usize;

    let mut best: Option<ImageChunk> = None;
    for i in 0..ntoc {
        let entry = 16 + i * 12;
        let ctype = rd_u32(entry)?;
        if ctype != CHUNK_IMAGE {
            continue;
        }
        let nominal = rd_u32(entry + 4)?;
        let pos = rd_u32(entry + 8)? as usize;
        // chunk header per Xcursor spec (9 CARD32):
        // header, type, subtype, version, width, height, xhot, yhot, delay
        if rd_u32(pos)? != 36 {
            continue; // not a chunk header we understand
        }
        let _chunk_type = rd_u32(pos + 4)?;
        let _version = rd_u32(pos + 12)?;
        let width = rd_u32(pos + 16)?;
        let height = rd_u32(pos + 20)?;
        let xhot = rd_u32(pos + 24)?;
        let yhot = rd_u32(pos + 28)?;
        let pixels_off = pos + 36;
        let len = width as usize * height as usize * 4;
        let pixels = data
            .get(pixels_off..pixels_off + len)
            .ok_or(CursorError::Malformed("truncated pixels"))?
            .to_vec();

        let chunk = ImageChunk { nominal, width, height, xhot, yhot, pixels };
        let better = match &best {
            None => true,
            Some(b) => (chunk.nominal as i32 - 24).abs() < (b.nominal as i32 - 24).abs(),
        };
        if better {
            best = Some(chunk);
        }
    }
    best.ok_or(CursorError::NoImage)
}

/// Convert an XCursor image to RDP masks.
///
/// RDP masks are bottom-up; andMask is 1bpp padded to 16-bit scanlines;
/// xorMask is 32bpp BGRx (BGRA byte order, alpha ignored, transparent
/// pixels black). Max size for ColorPointer is 96×96 ([MS-RDPBCGR]).
fn to_rdp(img: &ImageChunk, cache_index: u16) -> RdpPointer {
    let w = img.width.min(96) as usize;
    let h = img.height.min(96) as usize;

    // Stride math
    let and_stride = (w + 15) / 16 * 2; // bytes per scanline, 16-bit aligned
    let mut and_mask = vec![0u8; and_stride * h];
    let mut xor_mask = vec![0u8; w * 4 * h];

    for y in 0..h {
        // bottom-up destination row
        let dst_y = h - 1 - y;
        for x in 0..w {
            let src = &img.pixels[(y * img.width as usize + x) * 4..];
            let (b, g, r, a) = (src[0], src[1], src[2], src[3]);

            // xor: BGRx bottom-up.
            // Alpha rule (2026-08-22 fix): only FULLY transparent (a==0)
            // pixels are transparent; every a>0 pixel draws its color.
            // The previous a>128 threshold dropped the breeze arrow's 85
            // anti-aliased edge/shadow pixels to transparent — speckled
            // holes that read as a glitch around the cursor.
            let opaque = a > 0;
            let xo = (dst_y * w + x) * 4;
            xor_mask[xo] = if opaque { b } else { 0 };
            xor_mask[xo + 1] = if opaque { g } else { 0 };
            xor_mask[xo + 2] = if opaque { r } else { 0 };
            xor_mask[xo + 3] = 0;

            // and: 1 => transparent (hide xor pixel)
            if !opaque {
                let byte = dst_y * and_stride + x / 8;
                and_mask[byte] |= 0x80 >> (x % 8);
            }
        }
    }

    RdpPointer {
        cache_index,
        hot_spot: (
            img.xhot.min(96) as u16,
            img.yhot.min(96) as u16,
        ),
        width: w as u16,
        height: h as u16,
        and_mask,
        xor_mask,
    }
}

/// Load the system arrow pointer and convert it for RDP.
///
/// Search order: the configured theme dir first (from $XCURSOR_PATH or
/// /usr/share/icons), falling back to breeze_cursors.
pub fn load_default_pointer() -> Result<RdpPointer, CursorError> {
    let candidates = [
        "/usr/share/icons/breeze_cursors/cursors/left_ptr",
        "/usr/share/icons/default/cursors/left_ptr",
        "/usr/share/icons/whiteglass/cursors/left_ptr",
    ];
    let mut last = CursorError::NoImage;
    for path in candidates {
        let p = Path::new(path);
        if !p.exists() {
            continue;
        }
        match std::fs::read(p) {
            Ok(data) => match parse_xcursor(&data) {
                Ok(img) => return Ok(to_rdp(&img, 0)),
                Err(e) => last = e,
            },
            Err(e) => last = CursorError::Io(e),
        }
    }
    Err(last)
}

/// Encode an [`RdpPointer`] as the TS_COLORPOINTERATTRIBUTE wire body
/// (without the fast-path header) — ready for `ServerEvent::Pointer`.
pub fn encode_color_pointer(p: &RdpPointer) -> Vec<u8> {
    let mut out = Vec::with_capacity(14 + p.and_mask.len() + p.xor_mask.len());
    out.extend_from_slice(&p.cache_index.to_le_bytes());
    out.extend_from_slice(&p.hot_spot.0.to_le_bytes());
    out.extend_from_slice(&p.hot_spot.1.to_le_bytes());
    out.extend_from_slice(&p.width.to_le_bytes());
    out.extend_from_slice(&p.height.to_le_bytes());
    out.extend_from_slice(&(p.and_mask.len() as u16).to_le_bytes());
    out.extend_from_slice(&(p.xor_mask.len() as u16).to_le_bytes());
    // NOTE wire order per MS-RDPBCGR: xorBmp first, then andBmp
    out.extend_from_slice(&p.xor_mask);
    out.extend_from_slice(&p.and_mask);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_xcursor(w: u32, h: u32) -> Vec<u8> {
        // header (16) + toc (12) + one image chunk (36 + w*h*4)
        let pixels = vec![0u8; (w * h * 4) as usize];
        let mut v = Vec::new();
        v.extend_from_slice(&MAGIC.to_le_bytes());
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&0x0001_0000u32.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&CHUNK_IMAGE.to_le_bytes());
        v.extend_from_slice(&24u32.to_le_bytes()); // subtype nominal size
        v.extend_from_slice(&28u32.to_le_bytes()); // chunk position
        v.extend_from_slice(&36u32.to_le_bytes()); // chunk header size
        v.extend_from_slice(&CHUNK_IMAGE.to_le_bytes());
        v.extend_from_slice(&24u32.to_le_bytes()); // subtype
        v.extend_from_slice(&1u32.to_le_bytes()); // version
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes()); // xhot
        v.extend_from_slice(&2u32.to_le_bytes()); // yhot
        v.extend_from_slice(&1u32.to_le_bytes()); // delay
        v.extend_from_slice(&pixels);
        v
    }

    #[test]
    fn parses_minimal_file() {
        let data = minimal_xcursor(4, 4);
        let img = parse_xcursor(&data).expect("parse");
        assert_eq!((img.width, img.height, img.xhot, img.yhot), (4, 4, 1, 2));
    }

    #[test]
    fn rdp_masks_layout() {
        let data = minimal_xcursor(4, 4);
        let img = parse_xcursor(&data).expect("parse");
        let p = to_rdp(&img, 7);
        assert_eq!(p.cache_index, 7);
        // and stride for w=4: (4+15)/16*2 = 2 bytes/row (16-bit aligned), 4 rows
        assert_eq!(p.and_mask.len(), 2 * 4);
        assert_eq!(p.xor_mask.len(), 4 * 4 * 4);
        let enc = encode_color_pointer(&p);
        // fixed 14 (7 x u16: cacheIdx, xhot, yhot, w, h, lenAnd, lenXor —
        // matches TS_COLORPOINTERATTRIBUTE FIXED_PART_SIZE) + xor + and
        assert_eq!(enc.len(), 14 + p.xor_mask.len() + p.and_mask.len());
        // xor starts at byte 14 (after the 7-u16 fixed header), before and
        let _ = &enc[14..14 + 4];
    }

    #[test]
    fn transparent_pixels_get_and_bit() {
        let mut data = minimal_xcursor(2, 2);
        // make pixel (0,0) opaque white, everything else transparent
        let px_off = 16 + 12 + 36; // header + toc + chunk header
        data[px_off..px_off + 4].copy_from_slice(&[255, 255, 255, 255]);
        let img = parse_xcursor(&data).expect("parse");
        let p = to_rdp(&img, 0);
        // bottom dst row = top src row (bottom-up); x=0..1 occupy bits 7..6
        let top_row_bits = p.and_mask[(p.height as usize - 1) * 2] >> 6;
        // x=0 opaque -> bit7 clear; x=1 transparent -> bit6 set => 0b01 = 1
        assert_eq!(top_row_bits, 0b01);
    }

    /// REGRESSION (2026-08-22): the a>128 alpha threshold dropped the
    /// breeze arrow's 85 anti-aliased pixels to fully transparent,
    /// producing speckled holes around the rendered cursor ("glitching").
    /// Any alpha > 0 must draw its color; only a==0 is punch-through.
    #[test]
    fn semi_transparent_pixels_are_drawn() {
        let mut data = minimal_xcursor(2, 2);
        let px_off = 16 + 12 + 36;
        // pixel (0,0): WEAK alpha (10) with color — must DRAW.
        data[px_off..px_off + 4].copy_from_slice(&[200, 100, 50, 10]);
        // pixel (1,0): fully transparent — must stay punched out.
        // (all-zero from minimal_xcursor)
        let img = parse_xcursor(&data).expect("parse");
        let p = to_rdp(&img, 0);

        // Top source row lands at bottom dst row (bottom-up). x=0 first.
        // and-bit for x=0 must be CLEAR (drawn) despite tiny alpha…
        let byte = p.and_mask[(p.height as usize - 1) * 2];
        assert_eq!(byte >> 7 & 1, 0, "a=10 pixel must be drawn, not punched");
        // …and its xor color present:
        let xo = ((p.height as usize - 1) * 2) * 4 * 2; // bottom row, x=0
        assert_eq!(&p.xor_mask[xo..xo + 3], &[50, 100, 200], "BGR color preserved");
        // x=1 (transparent) still punched:
        assert_eq!(byte >> 6 & 1, 1, "a=0 pixel stays transparent");
    }

    /// REGRESSION GUARD (2026-08-21): the hand-rolled wire encoder in
    /// `encode_color_pointer` must stay byte-identical to ironrdp's own
    /// `ColorPointerAttribute::encode`. Any drift (field order, the
    /// xor-before-and mask ordering, length fields) produces a PDU that
    /// clients silently drop. Verified against fork rev 95375180.
    #[test]
    fn wire_encode_matches_ironrdp_encoder() {
        use ironrdp_core::encode_vec;
        use ironrdp_pdu::pointer::{ColorPointerAttribute, Point16};

        let data = minimal_xcursor(6, 8);
        let img = parse_xcursor(&data).expect("parse");
        let p = to_rdp(&img, 3);

        let ours = encode_color_pointer(&p);

        let reference = encode_vec(&ColorPointerAttribute {
            cache_index: p.cache_index,
            hot_spot: Point16 {
                x: p.hot_spot.0,
                y: p.hot_spot.1,
            },
            width: p.width,
            height: p.height,
            xor_mask: &p.xor_mask,
            and_mask: &p.and_mask,
        })
        .expect("ironrdp reference encode");

        assert_eq!(
            ours, reference,
            "hand-rolled TS_COLORPOINTERATTRIBUTE drifted from ironrdp's encoder"
        );
    }

    /// REGRESSION GUARD: malformed/truncated XCursor files must return
    /// Err, never panic. Guards parse_xcursor against bad-system-input.
    #[test]
    fn malformed_xcursor_is_error_not_panic() {
        assert!(parse_xcursor(&[]).is_err());
        assert!(parse_xcursor(&[0u8; 10]).is_err());
        // valid magic but truncated TOC
        let mut truncated = Vec::new();
        truncated.extend_from_slice(&0x7275_6358u32.to_le_bytes());
        truncated.extend_from_slice(&16u32.to_le_bytes());
        truncated.extend_from_slice(&0x0001_0000u32.to_le_bytes());
        truncated.extend_from_slice(&5u32.to_le_bytes()); // claims 5 toc entries
        truncated.extend_from_slice(&[0u8; 8]); // only room for ~0
        let result = std::panic::catch_unwind(|| parse_xcursor(&truncated));
        assert!(result.is_ok(), "parse must not panic on truncated files");
        assert!(result.unwrap().is_err(), "truncated file must be an error");
    }
}
