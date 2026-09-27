//! `img` — images for userspace programs: icons, cursors, pictures.
//!
//! [`Image`] holds **premultiplied** `0xAARRGGBB` pixels, the format
//! [`draw::Canvas::blit_over`] composites (its AVX2 path included). Every
//! PNG colour type and depth decodes to it: palette (with `tRNS`), grey,
//! grey + alpha, RGB, RGBA, 1–16 bits, interlaced or not; 16-bit channels
//! are rounded to 8.
//!
//! The decoding is `zune-png`'s; what is ours is the conversion and
//! the limits. `no_std` + `alloc`, no syscalls: the caller reads the file
//! (`userspace::img::load`).
//!
//! [`draw::Canvas::blit_over`]: ../draw/canvas/struct.Canvas.html#method.blit_over

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::fmt;

use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_png::error::PngDecodeErrors;
use zune_png::PngDecoder;

/// Largest width or height accepted: a hostile or broken header must not
/// make a program allocate gigabytes. 8192² × 4 bytes is 256 MiB already.
pub const MAX_SIDE: usize = 8192;

/// A decoded image: `w x h` premultiplied `0xAARRGGBB` pixels, rows `w`
/// apart.
#[derive(Clone, PartialEq, Eq)]
pub struct Image {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
}

impl fmt::Debug for Image {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Image {}x{}", self.w, self.h)
    }
}

#[derive(Debug)]
pub enum Error {
    /// Not a PNG, a corrupt one, or one using something unsupported
    /// (APNG frames beyond the first are ignored, not refused).
    Png(PngDecodeErrors),
    /// Wider or taller than [`MAX_SIDE`].
    TooLarge(usize, usize),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Png(e) => write!(f, "png: {:?}", e),
            Error::TooLarge(w, h) => write!(f, "{}x{} exceeds {}x{}", w, h, MAX_SIDE, MAX_SIDE),
        }
    }
}

impl From<PngDecodeErrors> for Error {
    fn from(e: PngDecodeErrors) -> Error {
        Error::Png(e)
    }
}

/// Decodes a PNG file's bytes.
pub fn decode_png(bytes: &[u8]) -> Result<Image, Error> {
    // 8-bit output always, and an alpha channel always (from tRNS when
    // there is one, else opaque): only two layouts to convert, RGBA and
    // grey + alpha.
    let options = DecoderOptions::default()
        .png_set_strip_to_8bit(true)
        .png_set_add_alpha_channel(true);
    let mut dec = PngDecoder::new_with_options(ZCursor::new(bytes), options);
    dec.decode_headers()?;
    let (w, h) = dec.dimensions().ok_or(PngDecodeErrors::GenericStatic("no dimensions"))?;
    if w > MAX_SIDE || h > MAX_SIDE {
        return Err(Error::TooLarge(w, h));
    }
    let colorspace = dec.colorspace().ok_or(PngDecodeErrors::GenericStatic("no colorspace"))?;
    let raw = match dec.decode()? {
        zune_core::result::DecodingResult::U8(v) => v,
        _ => return Err(PngDecodeErrors::GenericStatic("expected 8-bit output").into()),
    };
    let px = match colorspace {
        ColorSpace::RGBA => convert::<4>(&raw, w * h, |p| (p[0], p[1], p[2], p[3])),
        ColorSpace::LumaA => convert::<2>(&raw, w * h, |p| (p[0], p[0], p[0], p[1])),
        _ => return Err(PngDecodeErrors::GenericStatic("unexpected colorspace").into()),
    };
    let px = px.ok_or(PngDecodeErrors::GenericStatic("short pixel data"))?;
    Ok(Image { w, h, px })
}

/// `n` pixels of `N` bytes each, through `split` into `(r, g, b, a)`.
fn convert<const N: usize>(raw: &[u8], n: usize, split: impl Fn(&[u8]) -> (u8, u8, u8, u8)) -> Option<Vec<u32>> {
    let raw = raw.get(..n * N)?;
    Some(raw.chunks_exact(N).map(|p| {
        let (r, g, b, a) = split(p);
        premultiply(r, g, b, a)
    }).collect())
}

/// Straight-alpha channels to one premultiplied `0xAARRGGBB` pixel, each
/// colour rounded to nearest (`c·a/255`).
pub fn premultiply(r: u8, g: u8, b: u8, a: u8) -> u32 {
    let m = |c: u8| {
        let t = c as u32 * a as u32 + 128;
        (t + (t >> 8)) >> 8
    };
    (a as u32) << 24 | m(r) << 16 | m(g) << 8 | m(b)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;

    /// Encodes `data` with the `png` crate.
    fn encode(w: u32, h: u32, color: png::ColorType, depth: png::BitDepth, data: &[u8], setup: impl FnOnce(&mut png::Encoder<&mut Vec<u8>>)) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(color);
            enc.set_depth(depth);
            setup(&mut enc);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(data).unwrap();
        }
        out
    }

    #[test]
    fn premultiply_rounds_to_nearest() {
        for a in 0..=255u32 {
            for c in 0..=255u32 {
                let want = ((c * a) as f64 / 255.0).round() as u32;
                assert_eq!(premultiply(c as u8, 0, 0, a as u8) >> 16 & 0xFF, want, "c={c} a={a}");
            }
        }
        assert_eq!(premultiply(0x12, 0x34, 0x56, 255), 0xFF123456);
        assert_eq!(premultiply(0xFF, 0xFF, 0xFF, 0), 0);
    }

    #[test]
    fn rgba8_is_premultiplied() {
        let data = [255, 0, 0, 255, 255, 255, 255, 128, 10, 20, 30, 0, 0, 0, 255, 64];
        let png = encode(2, 2, png::ColorType::Rgba, png::BitDepth::Eight, &data, |_| {});
        let img = decode_png(&png).unwrap();
        assert_eq!((img.w, img.h), (2, 2));
        assert_eq!(img.px, vec![0xFFFF0000, 0x80808080, 0x00000000, 0x40000040]);
    }

    #[test]
    fn rgb8_is_opaque() {
        let data = [1, 2, 3, 250, 251, 252];
        let png = encode(2, 1, png::ColorType::Rgb, png::BitDepth::Eight, &data, |_| {});
        assert_eq!(decode_png(&png).unwrap().px, vec![0xFF010203, 0xFFFAFBFC]);
    }

    #[test]
    fn grey_and_grey_alpha() {
        let png = encode(2, 1, png::ColorType::Grayscale, png::BitDepth::Eight, &[0x10, 0xF0], |_| {});
        assert_eq!(decode_png(&png).unwrap().px, vec![0xFF101010, 0xFFF0F0F0]);
        let png = encode(2, 1, png::ColorType::GrayscaleAlpha, png::BitDepth::Eight, &[0xFF, 0x80, 0x40, 0], |_| {});
        assert_eq!(decode_png(&png).unwrap().px, vec![0x80808080, 0]);
    }

    #[test]
    fn palette_with_trns() {
        // Index 0 transparent, 1 half-transparent green, 2 opaque blue;
        // 2 bits per pixel, so a row of 3 pixels is one byte.
        let png = encode(3, 1, png::ColorType::Indexed, png::BitDepth::Two, &[0b00_01_10_00], |e| {
            e.set_palette(vec![255, 0, 0, 0, 255, 0, 0, 0, 255]);
            e.set_trns(vec![0, 128]);
        });
        assert_eq!(decode_png(&png).unwrap().px, vec![0, 0x80008000, 0xFF0000FF]);
    }

    #[test]
    fn sixteen_bit_rounds_to_eight() {
        // 0xFFFF opaque white, then 0x8080 grey at alpha 0xFFFF.
        let data = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0xFF, 0xFF];
        let png = encode(2, 1, png::ColorType::Rgba, png::BitDepth::Sixteen, &data, |_| {});
        assert_eq!(decode_png(&png).unwrap().px, vec![0xFFFFFFFF, 0xFF808080]);
    }

    #[test]
    fn a_larger_image_survives_every_filter() {
        // Enough rows and noise for the encoder's adaptive filtering to
        // pick all five filter types.
        let (w, h) = (67u32, 41u32);
        let mut v = 0x2545F491u32;
        let data: Vec<u8> = (0..w * h * 4).map(|i| {
            v ^= v << 13;
            v ^= v >> 17;
            v ^= v << 5;
            if i % 7 == 0 { v as u8 } else { (i / 4 % w) as u8 }
        }).collect();
        let png = encode(w, h, png::ColorType::Rgba, png::BitDepth::Eight, &data, |e| {
            e.set_filter(png::Filter::Adaptive);
        });
        let img = decode_png(&png).unwrap();
        let want: Vec<u32> = data.chunks_exact(4).map(|p| premultiply(p[0], p[1], p[2], p[3])).collect();
        assert_eq!(img.px, want);
    }

    #[test]
    fn garbage_and_truncation_are_errors() {
        assert!(decode_png(b"not a png").is_err());
        let png = encode(4, 4, png::ColorType::Rgb, png::BitDepth::Eight, &[7; 48], |_| {});
        for cut in [8, 20, 33, png.len() - 13] {
            assert!(decode_png(&png[..cut]).is_err(), "cut at {cut}");
        }
    }

    /// CRC-32 (ISO-HDLC, the one PNG chunks carry).
    fn crc32(bytes: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in bytes {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ c >> 1 } else { c >> 1 };
            }
        }
        !c
    }

    #[test]
    fn oversized_header_is_refused_before_allocating() {
        let mut png = encode(1, 1, png::ColorType::Rgb, png::BitDepth::Eight, &[0; 3], |_| {});
        // IHDR: length at 8, type at 12, width 16..20, height 20..24, CRC
        // over type + data at 29..33. A valid header, so only the limit
        // can stop it — before the ~1 GiB output buffer is allocated.
        let side = (MAX_SIDE as u32 + 1).to_be_bytes();
        png[16..20].copy_from_slice(&side);
        png[20..24].copy_from_slice(&side);
        let crc = crc32(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        match decode_png(&png) {
            Err(Error::TooLarge(w, h)) => assert_eq!((w, h), (MAX_SIDE + 1, MAX_SIDE + 1)),
            other => panic!("expected TooLarge, got {:?}", other),
        }
    }
}
