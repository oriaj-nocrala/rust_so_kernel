//! The painter (feature `render`): [`Paint`] operations into `0x00RRGGBB` pixels, text with the `text` crate (Noto, antialiased), which
//! also measures it ([`Measure`]). The program reads the font files (`FONT_FILES` under `/mnt/usr/share/fonts`) and hands their bytes in.

use alloc::vec::Vec;

use draw::Canvas;
use text::{Fonts, GlyphCache, Style, MONO, SANS};

use crate::{Font, Measure, Paint, Rect};

/// The files a [`Painter`] wants (regular and bold sans, regular mono), as the disk has them.
pub const FONT_FILES: [&str; 3] = ["NotoSans-Regular.ttf", "NotoSans-Bold.ttf", "NotoSansMono-Regular.ttf"];

pub struct Painter {
    fonts: Fonts,
    cache: GlyphCache,
    size: f32,
    line_h: i32,
}

impl Painter {
    /// Text at `size` pixels from these font files' bytes. `None` if none of them is a font.
    pub fn new(files: Vec<Vec<u8>>, size: f32) -> Option<Painter> {
        let mut fonts = Fonts::new();
        let mut any = false;
        for f in files {
            any |= !fonts.add(f).is_empty();
        }
        if !any {
            return None;
        }
        let line_h = fonts.measure("Ag", &Style::new(SANS, size), None).1.max(1);
        Some(Painter { fonts, cache: GlyphCache::default(), size, line_h })
    }

    fn style(&self, font: Font) -> Style<'static> {
        match font {
            Font::Sans => Style::new(SANS, self.size),
            Font::SansBold => Style::new(SANS, self.size).bold(),
            Font::Mono => Style::new(MONO, self.size),
        }
    }

    /// Draws `ops` into `px` (`w x h`, rows `w` long).
    pub fn paint(&mut self, ops: &[Paint], px: &mut [u32], w: usize, h: usize) {
        let all = Rect::new(0, 0, w as i32, h as i32);
        let mut clip = all;
        for op in ops {
            match op {
                Paint::Clip(r) => clip = r.intersect(&all).unwrap_or(Rect::new(0, 0, 0, 0)),
                Paint::Fill { rect, color } => {
                    let Some(r) = rect.intersect(&clip) else { continue };
                    for y in r.y..r.bottom() {
                        let row = y as usize * w;
                        px[row + r.x as usize..row + r.right() as usize].fill(*color);
                    }
                }
                Paint::Button { rect, look, down } => {
                    if !clip.is_empty() {
                        look.paint(*down, px, w, clip, *rect, 1);
                    }
                }
                Paint::Text { x, y, text, font, color } => {
                    if clip.is_empty() {
                        continue;
                    }
                    // a canvas over the clip, so the glyphs stop at its edges
                    let off = clip.y as usize * w + clip.x as usize;
                    let len = (clip.h as usize - 1) * w + clip.w as usize;
                    let mut cv = Canvas::new(&mut px[off..off + len], clip.w as usize, clip.h as usize, w);
                    let l = self.fonts.layout(text, &self.style(*font).color(*color), None);
                    self.cache.draw(&mut cv, &l, x - clip.x, y - clip.y);
                }
                Paint::Image { rect, pixels, w: iw, .. } => {
                    let Some(r) = rect.intersect(&clip) else { continue };
                    for y in r.y..r.bottom() {
                        let src = (y - rect.y) as usize * iw + (r.x - rect.x) as usize;
                        let dst = y as usize * w + r.x as usize;
                        px[dst..dst + r.w as usize].copy_from_slice(&pixels[src..src + r.w as usize]);
                    }
                }
            }
        }
    }
}

impl Measure for Painter {
    fn width(&mut self, s: &str, font: Font) -> i32 {
        if s.is_empty() {
            return 0;
        }
        let st = self.style(font);
        self.fonts.measure(s, &st, None).0
    }

    fn line_height(&mut self) -> i32 {
        self.line_h
    }
}
