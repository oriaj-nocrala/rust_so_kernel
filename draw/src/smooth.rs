//! Antialiased text: Noto Sans Mono, pre-rasterised by
//! `noto-sans-mono-bitmap` (the kernel console's and `vt`'s font), drawn
//! by blending each glyph's coverage over the pixels already there — so
//! text sits on gradients, graphs and panels, not only on a flat colour
//! (`vt::render` fills the cell background; this does not).
//!
//! Basic Latin only; anything else draws as `?`. Sizes 16, 20, 24 and 32
//! pixels, regular and bold.

use noto_sans_mono_bitmap::{get_raster, get_raster_width};

pub use noto_sans_mono_bitmap::{FontWeight, RasterHeight};

use crate::color::mix;
use crate::Canvas;

/// A size and weight of the font. Every glyph is one cell wide (monospace).
#[derive(Clone, Copy, Debug)]
pub struct Smooth {
    size: RasterHeight,
    weight: FontWeight,
}

impl Smooth {
    pub const fn new(size: RasterHeight, weight: FontWeight) -> Smooth {
        Smooth { size, weight }
    }

    /// Cell size in pixels, `(w, h)`.
    pub fn cell(&self) -> (i32, i32) {
        (get_raster_width(self.weight, self.size) as i32, self.size.val() as i32)
    }

    /// Width of `s` in pixels.
    pub fn width(&self, s: &str) -> i32 {
        s.chars().count() as i32 * self.cell().0
    }
}

impl Canvas<'_> {
    /// Draws `s` with the top left of its first cell at `(x, y)`, blended
    /// over the canvas in colour `c`. Clipped like every primitive. Returns
    /// the x just past the last glyph, for continuing on the same line.
    pub fn smooth_text(&mut self, f: Smooth, x: i32, y: i32, s: &str, c: u32) -> i32 {
        let (cw, _) = f.cell();
        let mut gx = x;
        for ch in s.chars() {
            if ch != ' ' {
                let glyph = get_raster(ch, f.weight, f.size).or_else(|| get_raster('?', f.weight, f.size));
                if let Some(g) = glyph {
                    for (row, cov) in g.raster().iter().enumerate() {
                        for (col, &a) in cov.iter().enumerate() {
                            if a == 0 {
                                continue;
                            }
                            let (px, py) = (gx + col as i32, y + row as i32);
                            if let Some(under) = self.get(px, py) {
                                // 0..=255 coverage onto mix's 0..=256.
                                let t = a as i32 + (a as i32 >> 7);
                                self.put(px, py, mix(under, c, t));
                            }
                        }
                    }
                }
            }
            gx += cw;
        }
        gx
    }

    /// [`smooth_text`](Self::smooth_text) right-aligned: the text ends at `right`.
    pub fn smooth_text_right(&mut self, f: Smooth, right: i32, y: i32, s: &str, c: u32) -> i32 {
        self.smooth_text(f, right - f.width(s), y, s, c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const F: Smooth = Smooth::new(RasterHeight::Size16, FontWeight::Regular);

    #[test]
    fn full_coverage_is_the_colour_and_empty_coverage_keeps_the_background() {
        let (cw, ch) = F.cell();
        let (w, h) = (cw as usize, ch as usize);
        let mut px = [0x0010_2030u32; 64 * 64];
        let mut cv = Canvas::new(&mut px, w, h, 64);
        cv.smooth_text(F, 0, 0, "#", 0xFF_FFFF);
        let raster = get_raster('#', FontWeight::Regular, RasterHeight::Size16).unwrap();
        for (y, row) in raster.raster().iter().enumerate() {
            for (x, &a) in row.iter().enumerate() {
                let p = px[y * 64 + x];
                match a {
                    0 => assert_eq!(p, 0x0010_2030, "({x},{y})"),
                    255 => assert_eq!(p, 0xFF_FFFF, "({x},{y})"),
                    // Partial coverage lands between the two, channel by
                    // channel (a faint edge may round to the background).
                    _ => {
                        let (r, g, b) = crate::color::channels(p);
                        assert!((0x10..=0xFF).contains(&r) && (0x20..=0xFF).contains(&g) && (0x30..=0xFF).contains(&b), "({x},{y}) a={a}");
                        if a >= 128 {
                            assert!(r > 0x80, "({x},{y}) a={a}: half coverage is at least half way");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn it_blends_over_what_is_there_not_a_flat_background() {
        let (cw, ch) = F.cell();
        let mut px = [0u32; 64 * 64];
        for (i, p) in px.iter_mut().enumerate() {
            *p = if i % 2 == 0 { 0x0000_00FF } else { 0x00FF_0000 };
        }
        let before = px;
        Canvas::new(&mut px, cw as usize, ch as usize, cw as usize).smooth_text(F, 0, 0, "-", 0x00_FF00);
        let raster = get_raster('-', FontWeight::Regular, RasterHeight::Size16).unwrap();
        for (y, row) in raster.raster().iter().enumerate() {
            for (x, &a) in row.iter().enumerate() {
                let i = y * cw as usize + x;
                if a == 0 {
                    assert_eq!(px[i], before[i]);
                }
            }
        }
    }

    #[test]
    fn clips_and_reports_the_advance() {
        let (cw, _) = F.cell();
        let mut px = [0u32; 10 * 10];
        let mut cv = Canvas::new(&mut px, 10, 10, 10);
        assert_eq!(cv.smooth_text(F, -5, -7, "Hi?", 0xFFFFFF), -5 + 3 * cw);
        assert_eq!(cv.smooth_text_right(F, 10, 0, "ab", 0xFFFFFF), 10);
        assert_eq!(F.width("abc"), 3 * cw);
        // Unknown characters draw as '?' rather than nothing.
        let mut a = [0u32; 32 * 32];
        let mut b = [0u32; 32 * 32];
        Canvas::new(&mut a, 32, 32, 32).smooth_text(F, 0, 0, "\u{263A}", 0xFFFFFF);
        Canvas::new(&mut b, 32, 32, 32).smooth_text(F, 0, 0, "?", 0xFFFFFF);
        assert_eq!(a, b);
    }
}
