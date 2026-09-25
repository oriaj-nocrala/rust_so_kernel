//! A grid's damaged rows into a `&mut [u32]` of `0x00RRGGBB` pixels.
//!
//! The glyphs are the kernel console's: Noto Sans Mono from
//! `noto-sans-mono-bitmap`, antialiased, blended foreground over
//! background with the same linear formula as `Framebuffer::draw_glyph`.
//! The grid starts at pixel (0, 0) of `dst`; a caller that wants a margin
//! passes the sub-slice that starts there. Nothing outside the grid's
//! `cols * w` by `rows * h` pixels is written — `stride` padding included.

use noto_sans_mono_bitmap::{get_raster, get_raster_width, FontWeight};

pub use noto_sans_mono_bitmap::RasterHeight;

use crate::grid::{Damage, Grid};

#[derive(Clone, Copy, Debug)]
pub struct Font {
    size: RasterHeight,
    w: usize,
    h: usize,
}

impl Font {
    pub const fn new(size: RasterHeight) -> Self {
        Font { size, w: get_raster_width(FontWeight::Regular, size), h: size.val() }
    }

    /// The size the kernel console picks for a screen this tall
    /// (`framebuffer_console::pick_font`): about 45-54 rows of text.
    pub const fn for_screen_height(height: usize) -> Self {
        Font::new(match height {
            0..=719 => RasterHeight::Size16,
            720..=999 => RasterHeight::Size20,
            1000..=1399 => RasterHeight::Size24,
            _ => RasterHeight::Size32,
        })
    }

    /// Cell size in pixels, `(w, h)`.
    pub const fn cell(&self) -> (usize, usize) {
        (self.w, self.h)
    }
}

/// A rectangle in pixels, for the surface's `damage` request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelRect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

/// Draw the rows in `damage`, with the cursor as a reversed cell when
/// `show_cursor` (the caller's blink phase) and the grid's `?25` both say
/// so. Returns the pixels written, or `None` for no damage.
///
/// Panics if `dst`/`stride` cannot hold the grid: that is a sizing bug in
/// the caller, not something to clip silently.
pub fn render(
    grid: &Grid,
    damage: &Damage,
    font: &Font,
    show_cursor: bool,
    dst: &mut [u32],
    stride: usize,
) -> Option<PixelRect> {
    let (first, last) = damage.span()?;
    let (cw, ch) = font.cell();
    let width = grid.cols() * cw;
    assert!(stride >= width, "stride {stride} narrower than the grid's {width} pixels");
    assert!(
        dst.len() >= stride * (grid.rows() * ch - 1) + width,
        "buffer too small for {}x{} cells of {cw}x{ch}",
        grid.cols(),
        grid.rows()
    );

    let cursor = (show_cursor && grid.cursor_visible()).then(|| grid.cursor());
    for row in damage.rows().filter(|&r| r < grid.rows()) {
        for col in 0..grid.cols() {
            let cell = grid.cell(row, col);
            let (mut fg, mut bg) = if cell.attrs.reverse { (cell.bg, cell.fg) } else { (cell.fg, cell.bg) };
            if cursor == Some((row, col)) {
                core::mem::swap(&mut fg, &mut bg);
            }
            let weight = if cell.attrs.bold { FontWeight::Bold } else { FontWeight::Regular };
            let origin = row * ch * stride + col * cw;
            let raster = match cell.ch {
                ' ' => None,
                c => get_raster(c, weight, font.size).or_else(|| get_raster('?', weight, font.size)),
            };
            match raster {
                Some(glyph) => draw_glyph(dst, origin, stride, cw, ch, glyph.raster(), fg, bg),
                None => fill(dst, origin, stride, cw, ch, bg),
            }
        }
    }
    Some(PixelRect { x: 0, y: first * ch, w: width, h: (last - first) * ch })
}

fn fill(dst: &mut [u32], origin: usize, stride: usize, w: usize, h: usize, px: u32) {
    for y in 0..h {
        dst[origin + y * stride..][..w].fill(px);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_glyph(dst: &mut [u32], origin: usize, stride: usize, w: usize, h: usize, raster: &[&[u8]], fg: u32, bg: u32) {
    for y in 0..h {
        let cov: &[u8] = raster.get(y).copied().unwrap_or(&[]);
        let line = &mut dst[origin + y * stride..][..w];
        for (x, px) in line.iter_mut().enumerate() {
            *px = match cov.get(x).copied().unwrap_or(0) {
                0 => bg,
                255 => fg,
                a => blend(fg, bg, a),
            };
        }
    }
}

/// `bg` moved `a / 255` of the way towards `fg`, per channel — the
/// kernel's `framebuffer::blend`.
fn blend(fg: u32, bg: u32, a: u8) -> u32 {
    let a = a as u32;
    let mix = |shift: u32| {
        let (f, b) = ((fg >> shift) & 0xFF, (bg >> shift) & 0xFF);
        ((f * a + b * (255 - a) + 127) / 255) << shift
    };
    mix(16) | mix(8) | mix(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::{DEFAULT_BG, DEFAULT_FG};
    use crate::parser::Parser;
    use alloc::vec;
    use alloc::vec::Vec;

    const PAD: u32 = 0xAA_AAAA;
    const FONT: Font = Font::new(RasterHeight::Size16);

    /// A buffer wider than the grid, pre-filled, so padding can be checked.
    fn canvas(g: &Grid) -> (Vec<u32>, usize) {
        let (cw, ch) = FONT.cell();
        let stride = g.cols() * cw + 5;
        (vec![PAD; stride * g.rows() * ch], stride)
    }

    fn cell_pixels(dst: &[u32], stride: usize, row: usize, col: usize) -> Vec<u32> {
        let (cw, ch) = FONT.cell();
        (0..ch).flat_map(|y| dst[(row * ch + y) * stride + col * cw..][..cw].to_vec()).collect()
    }

    #[test]
    fn a_glyph_has_foreground_background_and_blended_pixels() {
        let mut g = Grid::new(3, 2);
        Parser::new().feed(&mut g, b"W");
        let (mut dst, stride) = canvas(&g);
        render(&g, &g.clone_damage_all(), &FONT, false, &mut dst, stride);
        let px = cell_pixels(&dst, stride, 0, 0);
        // Every pixel is the raster's coverage blended fg-over-bg.
        let glyph = get_raster('W', FontWeight::Regular, RasterHeight::Size16).unwrap();
        let (cw, ch) = FONT.cell();
        let expected: Vec<u32> = (0..ch)
            .flat_map(|y| (0..cw).map(move |x| (y, x)))
            .map(|(y, x)| match glyph.raster().get(y).and_then(|r| r.get(x)).copied().unwrap_or(0) {
                0 => DEFAULT_BG,
                a => blend(DEFAULT_FG, DEFAULT_BG, a),
            })
            .collect();
        assert_eq!(px, expected);
        assert!(px.contains(&DEFAULT_BG));
        assert!(px.iter().any(|&p| p != DEFAULT_BG), "ink");
        assert!(cell_pixels(&dst, stride, 0, 1).iter().all(|&p| p == DEFAULT_BG), "a blank is background");
    }

    #[test]
    fn the_padding_right_of_the_grid_is_never_written() {
        // The last column holds a glyph on one row and a blank on the
        // others: both paths (glyph and fill) end at the grid's edge.
        let mut g = Grid::new(5, 3);
        Parser::new().feed(&mut g, b"\x1b[41mabcd\r\nefghi\r\njk");
        let (mut dst, stride) = canvas(&g);
        let r = render(&g, &g.clone_damage_all(), &FONT, true, &mut dst, stride).unwrap();
        let (cw, ch) = FONT.cell();
        assert_eq!(r, PixelRect { x: 0, y: 0, w: 5 * cw, h: 3 * ch });
        for y in 0..3 * ch {
            assert!(dst[y * stride + 5 * cw..(y + 1) * stride].iter().all(|&p| p == PAD), "row {y}");
            assert!(dst[y * stride..y * stride + 5 * cw].iter().all(|&p| p != PAD), "row {y} drawn");
        }
    }

    #[test]
    fn only_damaged_rows_are_drawn() {
        let mut g = Grid::new(3, 4);
        let mut p = Parser::new();
        let _ = g.take_damage();
        p.feed(&mut g, b"\x1b[3;1Hx");
        let d = g.take_damage();
        let (mut dst, stride) = canvas(&g);
        let r = render(&g, &d, &FONT, false, &mut dst, stride).unwrap();
        let (_, ch) = FONT.cell();
        // Row 0 (where the cursor was) and row 2 (written, cursor now).
        assert_eq!((r.y, r.h), (0, 3 * ch));
        assert!(dst[ch * stride..2 * ch * stride].iter().all(|&p| p == PAD), "row 1 untouched");
        assert!(dst[3 * ch * stride..].iter().all(|&p| p == PAD), "row 3 untouched");
        assert!(dst[2 * ch * stride..3 * ch * stride].iter().any(|&p| p != DEFAULT_BG && p != PAD), "the x");
    }

    #[test]
    fn reverse_and_cursor_swap_the_colours() {
        let mut g = Grid::new(3, 1);
        Parser::new().feed(&mut g, b"\x1b[7m \x1b[27m");
        let (mut dst, stride) = canvas(&g);
        render(&g, &g.clone_damage_all(), &FONT, true, &mut dst, stride);
        assert!(cell_pixels(&dst, stride, 0, 0).iter().all(|&p| p == DEFAULT_FG), "reversed blank");
        assert!(cell_pixels(&dst, stride, 0, 1).iter().all(|&p| p == DEFAULT_FG), "the cursor");
        assert!(cell_pixels(&dst, stride, 0, 2).iter().all(|&p| p == DEFAULT_BG));

        // Blink phase off, or ?25l: no cursor.
        render(&g, &g.clone_damage_all(), &FONT, false, &mut dst, stride);
        assert!(cell_pixels(&dst, stride, 0, 1).iter().all(|&p| p == DEFAULT_BG));
        Parser::new().feed(&mut g, b"\x1b[?25l");
        render(&g, &g.clone_damage_all(), &FONT, true, &mut dst, stride);
        assert!(cell_pixels(&dst, stride, 0, 1).iter().all(|&p| p == DEFAULT_BG));
    }

    #[test]
    fn bold_uses_the_bold_face() {
        let mut g = Grid::new(2, 1);
        Parser::new().feed(&mut g, b"m\x1b[1mm");
        let (mut dst, stride) = canvas(&g);
        render(&g, &g.clone_damage_all(), &FONT, false, &mut dst, stride);
        let ink = |px: Vec<u32>| px.iter().filter(|&&p| p != DEFAULT_BG).count();
        assert!(ink(cell_pixels(&dst, stride, 0, 1)) > ink(cell_pixels(&dst, stride, 0, 0)));
    }

    #[test]
    fn characters_outside_the_font_draw_as_a_question_mark() {
        let mut g = Grid::new(2, 1);
        Parser::new().feed(&mut g, "€?".as_bytes());
        let (mut dst, stride) = canvas(&g);
        render(&g, &g.clone_damage_all(), &FONT, false, &mut dst, stride);
        assert_eq!(cell_pixels(&dst, stride, 0, 0), cell_pixels(&dst, stride, 0, 1));
    }

    #[test]
    fn no_damage_draws_nothing() {
        let mut g = Grid::new(2, 2);
        let _ = g.take_damage();
        let d = Damage::all(0);
        let (mut dst, stride) = canvas(&g);
        assert_eq!(render(&g, &d, &FONT, true, &mut dst, stride), None);
        assert!(dst.iter().all(|&p| p == PAD));
    }

    #[test]
    #[should_panic(expected = "buffer too small")]
    fn a_short_buffer_is_a_caller_bug() {
        let g = Grid::new(2, 2);
        let mut dst = vec![0; 10];
        render(&g, &Damage::all(2), &FONT, false, &mut dst, 1000);
    }

    #[test]
    fn blend_matches_the_kernel_formula() {
        assert_eq!(blend(0xFFFFFF, 0x000000, 128), 0x808080);
        assert_eq!(blend(0xFF0000, 0x0000FF, 255), 0xFF0000);
        assert_eq!(blend(0xFF0000, 0x0000FF, 0), 0x0000FF);
    }

    #[test]
    fn console_font_sizes() {
        assert_eq!(Font::for_screen_height(800).cell().1, 20);
        assert_eq!(Font::for_screen_height(1080).cell(), (11, 24));
    }

    impl Grid {
        fn clone_damage_all(&self) -> Damage {
            Damage::all(self.rows())
        }
    }
}
