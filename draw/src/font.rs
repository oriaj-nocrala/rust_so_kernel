//! Two pixel fonts and text drawing.
//!
//! [`FONT_3X5`] for small print, [`FONT_5X7`] for everything else; both
//! cover `0-9`, `A-Z` (lower case is drawn as upper case), space and
//! `! - / : . ,`. Each glyph is up to 7 rows of bits, the leftmost column in
//! the highest used bit. Text is drawn with each font pixel as a
//! `scale x scale` square, one blank column between glyphs.

use crate::canvas::Canvas;

#[derive(Clone, Copy)]
pub struct Font {
    /// Glyph width and height in font pixels.
    pub w: i32,
    pub h: i32,
    glyph: fn(u8) -> [u8; 7],
}

impl Font {
    /// The rows of `ch`, or blank for a character the font lacks.
    pub fn glyph(&self, ch: u8) -> [u8; 7] {
        (self.glyph)(ch.to_ascii_uppercase())
    }

    /// Width in pixels of `s` at `scale`.
    pub fn width(&self, s: &[u8], scale: i32) -> i32 {
        if s.is_empty() {
            return 0;
        }
        (s.len() as i32 * (self.w + 1) - 1) * scale
    }

    /// Height in pixels of a line at `scale`.
    pub fn height(&self, scale: i32) -> i32 {
        self.h * scale
    }
}

pub const FONT_3X5: Font = Font { w: 3, h: 5, glyph: glyph_3x5 };
pub const FONT_5X7: Font = Font { w: 5, h: 7, glyph: glyph_5x7 };

fn glyph_3x5(ch: u8) -> [u8; 7] {
    let g: [u8; 5] = match ch {
        b'0' => [7, 5, 5, 5, 7],
        b'1' => [2, 6, 2, 2, 7],
        b'2' => [7, 1, 7, 4, 7],
        b'3' => [7, 1, 7, 1, 7],
        b'4' => [5, 5, 7, 1, 1],
        b'5' => [7, 4, 7, 1, 7],
        b'6' => [7, 4, 7, 5, 7],
        b'7' => [7, 1, 2, 2, 2],
        b'8' => [7, 5, 7, 5, 7],
        b'9' => [7, 5, 7, 1, 7],
        b'A' => [2, 5, 7, 5, 5],
        b'B' => [6, 5, 6, 5, 6],
        b'C' => [3, 4, 4, 4, 3],
        b'D' => [6, 5, 5, 5, 6],
        b'E' => [7, 4, 6, 4, 7],
        b'F' => [7, 4, 6, 4, 4],
        b'G' => [3, 4, 5, 5, 3],
        b'H' => [5, 5, 7, 5, 5],
        b'I' => [7, 2, 2, 2, 7],
        b'J' => [1, 1, 1, 5, 2],
        b'K' => [5, 5, 6, 5, 5],
        b'L' => [4, 4, 4, 4, 7],
        b'M' => [5, 7, 7, 5, 5],
        b'N' => [6, 5, 5, 5, 5],
        b'O' => [2, 5, 5, 5, 2],
        b'P' => [6, 5, 6, 4, 4],
        b'Q' => [2, 5, 5, 6, 3],
        b'R' => [6, 5, 6, 5, 5],
        b'S' => [3, 4, 2, 1, 6],
        b'T' => [7, 2, 2, 2, 2],
        b'U' => [5, 5, 5, 5, 7],
        b'V' => [5, 5, 5, 5, 2],
        b'W' => [5, 5, 7, 7, 5],
        b'X' => [5, 5, 2, 5, 5],
        b'Y' => [5, 5, 2, 2, 2],
        b'Z' => [7, 1, 2, 4, 7],
        b':' => [0, 2, 0, 2, 0],
        b'!' => [2, 2, 2, 0, 2],
        b'-' => [0, 0, 7, 0, 0],
        b'/' => [1, 1, 2, 4, 4],
        b'.' => [0, 0, 0, 0, 2],
        b',' => [0, 0, 0, 2, 4],
        _ => [0; 5],
    };
    [g[0], g[1], g[2], g[3], g[4], 0, 0]
}

fn glyph_5x7(ch: u8) -> [u8; 7] {
    match ch {
        b'0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        b'1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        b'2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        b'3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        b'4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        b'5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        b'6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        b'7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        b'8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        b'9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        b'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        b'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        b'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        b'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
        b'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        b'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        b'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        b'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        b'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        b'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        b'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        b'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        b'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        b'N' => [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11],
        b'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        b'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        b'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        b'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        b'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        b'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        b'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        b'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        b'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A],
        b'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        b'Y' => [0x11, 0x11, 0x11, 0x0A, 0x04, 0x04, 0x04],
        b'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        b'!' => [0x04, 0x04, 0x04, 0x04, 0x04, 0x00, 0x04],
        b':' => [0x00, 0x0C, 0x0C, 0x00, 0x0C, 0x0C, 0x00],
        b'-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        b'/' => [0x01, 0x01, 0x02, 0x04, 0x08, 0x10, 0x10],
        b'.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C],
        b',' => [0x00, 0x00, 0x00, 0x00, 0x0C, 0x04, 0x08],
        _ => [0; 7],
    }
}

impl Canvas<'_> {
    /// Draws `s` with its top left at `(x, y)`; `color(i)` is glyph `i`'s
    /// colour (a closure so text can be striped or animated per letter).
    pub fn text(&mut self, font: Font, x: i32, y: i32, scale: i32, s: &[u8], color: impl Fn(usize) -> u32) {
        for (i, &ch) in s.iter().enumerate() {
            let rows = font.glyph(ch);
            let gx = x + i as i32 * (font.w + 1) * scale;
            let c = color(i);
            for (row, &bits) in rows.iter().take(font.h as usize).enumerate() {
                for col in 0..font.w {
                    if bits & (1 << (font.w - 1 - col)) != 0 {
                        self.rect(gx + col * scale, y + row as i32 * scale, scale, scale, c);
                    }
                }
            }
        }
    }

    /// [`text`](Self::text) over a drop shadow offset by half a font pixel
    /// (at least one pixel) down and right.
    pub fn text_shadowed(&mut self, font: Font, x: i32, y: i32, scale: i32, s: &[u8], shadow: u32, color: impl Fn(usize) -> u32) {
        let off = (scale / 2).max(1);
        self.text(font, x + off, y + off, scale, s, |_| shadow);
        self.text(font, x, y, scale, s, color);
    }

    /// The x at which `s` is horizontally centred on the canvas.
    pub fn center_x(&self, font: Font, s: &[u8], scale: i32) -> i32 {
        (self.width() - font.width(s, scale)) / 2
    }
}

/// The decimal digits of `n`, written into the end of `buf`.
pub fn digits(n: u32, buf: &mut [u8; 10]) -> &[u8] {
    let mut i = buf.len();
    let mut v = n;
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    &buf[i..]
}

#[cfg(test)]
mod tests {
    use super::*;

    const COVERED: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ!:-/.,";

    fn fonts() -> [Font; 2] {
        [FONT_3X5, FONT_5X7]
    }

    #[test]
    fn every_covered_glyph_has_ink_and_fits_its_box() {
        for f in fonts() {
            for &ch in COVERED {
                let g = f.glyph(ch);
                assert!(g.iter().any(|&r| r != 0), "{} blank in {}x{}", ch as char, f.w, f.h);
                for (row, &bits) in g.iter().enumerate() {
                    assert!(bits < 1 << f.w, "{} too wide", ch as char);
                    if row as i32 >= f.h {
                        assert_eq!(bits, 0, "{} too tall", ch as char);
                    }
                }
            }
        }
    }

    #[test]
    fn letters_and_digits_are_all_distinct() {
        for f in fonts() {
            let alnum = &COVERED[..36];
            for (i, &a) in alnum.iter().enumerate() {
                for &b in &alnum[i + 1..] {
                    assert_ne!(f.glyph(a), f.glyph(b), "{} == {} in {}x{}", a as char, b as char, f.w, f.h);
                }
            }
        }
    }

    #[test]
    fn unknown_characters_are_blank_and_lower_case_is_upper() {
        for f in fonts() {
            assert_eq!(f.glyph(b' '), [0; 7]);
            assert_eq!(f.glyph(b'~'), [0; 7]);
            assert_eq!(f.glyph(b'a'), f.glyph(b'A'));
        }
    }

    #[test]
    fn width_and_height() {
        assert_eq!(FONT_5X7.width(b"", 3), 0);
        assert_eq!(FONT_5X7.width(b"A", 1), 5);
        assert_eq!(FONT_5X7.width(b"AB", 2), 22);
        assert_eq!(FONT_3X5.width(b"ABC", 1), 11);
        assert_eq!(FONT_5X7.height(3), 21);
    }

    #[test]
    fn text_stays_inside_its_measured_box() {
        let mut px = [0u32; 64 * 32];
        let mut cv = Canvas::new(&mut px, 64, 32, 64);
        let s = b"W8M";
        let (x, y, sc) = (3, 4, 2);
        cv.text(FONT_5X7, x, y, sc, s, |_| 1);
        let (w, h) = (FONT_5X7.width(s, sc), FONT_5X7.height(sc));
        let mut ink = 0;
        for py in 0..32 {
            for px_ in 0..64 {
                if cv.get(px_, py) == Some(1) {
                    ink += 1;
                    assert!(px_ >= x && px_ < x + w && py >= y && py < y + h, "ink at ({}, {})", px_, py);
                }
            }
        }
        assert!(ink > 0);
    }

    #[test]
    fn text_colour_is_per_glyph() {
        let mut px = [0u32; 32 * 8];
        let mut cv = Canvas::new(&mut px, 32, 8, 32);
        cv.text(FONT_3X5, 0, 0, 1, b"II", |i| 10 + i as u32);
        assert_eq!(cv.get(0, 0), Some(10)); // I's top bar
        assert_eq!(cv.get(4, 0), Some(11));
    }

    #[test]
    fn shadow_sits_under_and_behind() {
        let mut px = [0u32; 16 * 16];
        let mut cv = Canvas::new(&mut px, 16, 16, 16);
        cv.text_shadowed(FONT_5X7, 0, 0, 2, b"I", 5, |_| 9);
        assert_eq!(cv.get(2, 0), Some(9)); // glyph
        assert_eq!(cv.get(8, 1), Some(5)); // shadow just past the top bar (x 2..=7)
    }

    #[test]
    fn center_x_centres() {
        let mut px = [0u32; 100 * 10];
        let cv = Canvas::new(&mut px, 100, 10, 100);
        assert_eq!(cv.center_x(FONT_5X7, b"AB", 1), (100 - 11) / 2);
    }

    #[test]
    fn digits_formats() {
        let mut b = [0u8; 10];
        assert_eq!(digits(0, &mut b), b"0");
        assert_eq!(digits(42, &mut b), b"42");
        assert_eq!(digits(u32::MAX, &mut b), b"4294967295");
    }
}
