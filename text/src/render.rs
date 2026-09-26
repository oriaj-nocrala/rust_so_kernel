//! From a [`TextLayout`] to pixels: `swash` rasterises each glyph into a
//! coverage mask at its size and sub-pixel phase, [`ClockCache`] keeps the
//! masks, and they are blended over the canvas in the run's colour.

use alloc::vec::Vec;

use draw::color::mix;
use draw::Canvas;
use parley::PositionedLayoutItem;
use swash::scale::{Render, ScaleContext, Scaler, Source};
use swash::zeno::{Format, Vector};
use swash::FontRef;

use crate::cache::{ClockCache, Stats};
use crate::{floor, TextLayout};

/// Horizontal positions are rounded to a quarter pixel: four masks per
/// glyph and size at most, and spacing that stays even at small sizes
/// (whole-pixel positions visibly bunch letters together).
const PHASES: i32 = 4;

/// What a cached mask costs beyond its pixels (key, placement, slot).
const OVERHEAD: usize = 48;

/// Default budget: every Latin-1 glyph of Noto Sans at a dozen sizes fits.
pub const DEFAULT_CACHE_BYTES: usize = 4 << 20;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct GlyphKey {
    /// `Blob::id()` of the font file: unique per registered file.
    font: u64,
    index: u32,
    glyph: u32,
    /// `f32::to_bits` of the size: sizes are compared exactly.
    size: u32,
    phase: u8,
}

/// A glyph's coverage, 0..=255 per pixel, and where it sits relative to
/// the pen position on the baseline (`left` right of it, `top` above it).
struct Mask {
    left: i32,
    top: i32,
    w: usize,
    h: usize,
    cov: Vec<u8>,
}

/// Rasterised glyphs, and the drawing of layouts with them. Owned by the
/// program (no global state); one is enough for any number of fonts.
pub struct GlyphCache {
    scale: ScaleContext,
    cache: ClockCache<GlyphKey, Mask>,
}

impl Default for GlyphCache {
    fn default() -> Self {
        Self::new(DEFAULT_CACHE_BYTES)
    }
}

impl GlyphCache {
    /// A cache holding at most `bytes` of masks.
    pub fn new(bytes: usize) -> GlyphCache {
        GlyphCache { scale: ScaleContext::new(), cache: ClockCache::new(bytes) }
    }

    pub fn stats(&self) -> Stats {
        self.cache.stats()
    }

    /// Draws `layout` with its top left at `(x, y)`, blended over what the
    /// canvas holds, clipped to it. Colours come from the layout's style.
    pub fn draw(&mut self, canvas: &mut Canvas, layout: &TextLayout, x: i32, y: i32) {
        for line in layout.inner.lines() {
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(run) = item else { continue };
                let color = run.style().brush;
                let font = run.run().font();
                let size = run.run().font_size();
                let Some(font_ref) = FontRef::from_index(font.data.data(), font.index as usize) else {
                    continue;
                };
                let font_id = font.data.id();
                // Per run: the context caches the parsed font by id, so
                // building one is a lookup and a size.
                let mut scaler =
                    self.scale.builder_with_id(font_ref, [font_id, font.index as u64]).size(size).hint(false).build();
                for g in run.positioned_glyphs() {
                    let gx = x as f32 + g.x;
                    let mut px = floor(gx);
                    let mut phase = ((gx - px as f32) * PHASES as f32 + 0.5) as i32;
                    if phase == PHASES {
                        px += 1;
                        phase = 0;
                    }
                    let py = y + floor(g.y + 0.5);
                    let key = GlyphKey {
                        font: font_id,
                        index: font.index,
                        glyph: g.id,
                        size: size.to_bits(),
                        phase: phase as u8,
                    };
                    if let Some(i) = self.cache.lookup(&key) {
                        blend(canvas, self.cache.at(i), px, py, color);
                        continue;
                    }
                    let mask = rasterise(&mut scaler, g.id, phase);
                    let cost = mask.cov.len() + OVERHEAD;
                    match self.cache.insert(key, mask, cost) {
                        Ok(i) => blend(canvas, self.cache.at(i), px, py, color),
                        Err(mask) => blend(canvas, &mask, px, py, color),
                    }
                }
            }
        }
    }
}

fn rasterise(scaler: &mut Scaler, glyph: u32, phase: i32) -> Mask {
    let offset = Vector::new(phase as f32 / PHASES as f32, 0.0);
    let image = Render::new(&[Source::Outline])
        .format(Format::Alpha)
        .offset(offset)
        .render(scaler, glyph as u16);
    match image {
        Some(img) => Mask {
            left: img.placement.left,
            top: img.placement.top,
            w: img.placement.width as usize,
            h: img.placement.height as usize,
            cov: img.data,
        },
        // No outline (a space) or a glyph swash cannot draw: nothing.
        None => Mask { left: 0, top: 0, w: 0, h: 0, cov: Vec::new() },
    }
}

/// `Framebuffer::draw_glyph`'s arithmetic: coverage `a` of 0..=255 mixes
/// the colour over what is there, through `Canvas`'s clipped accessors.
fn blend(canvas: &mut Canvas, m: &Mask, pen_x: i32, baseline: i32, color: u32) {
    let (x0, y0) = (pen_x + m.left, baseline - m.top);
    for row in 0..m.h {
        let cov = &m.cov[row * m.w..(row + 1) * m.w];
        for (col, &a) in cov.iter().enumerate() {
            if a == 0 {
                continue;
            }
            let (px, py) = (x0 + col as i32, y0 + row as i32);
            if let Some(under) = canvas.get(px, py) {
                let t = a as i32 + (a as i32 >> 7);
                canvas.put(px, py, mix(under, color, t));
            }
        }
    }
}
