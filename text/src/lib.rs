//! `text` — proportional, any-size, antialiased text for userspace programs
//! (`docs/gui/text-plan.md`).
//!
//! A thin layer over Linebender's stack: `parley` shapes (kerning, GPOS,
//! clusters), breaks lines (ICU4X rules) and handles bidi; `swash`
//! rasterises outlines into coverage masks. What is ours is small:
//!
//! - [`Fonts`]: fonts registered from bytes (no system font discovery —
//!   that needs `std`), and a small API to lay out and measure a string.
//! - [`TextLayout`]: the result, positioned lines of glyphs.
//! - [`GlyphCache`]: coverage masks keyed by (font, glyph, size, sub-pixel
//!   phase), bounded in bytes with CLOCK eviction ([`cache`]), and the
//!   compositing of a layout onto a [`draw::Canvas`], clipped, blended over
//!   whatever is there.
//!
//! `no_std` + `alloc`, no syscalls: the caller reads the font files and
//! owns the pixels (`userspace::gfx`). Coordinates are pixels, `f32` where
//! `parley` gives them, `i32` once they land on the canvas.

#![no_std]

extern crate alloc;

pub mod cache;
mod render;

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use parley::fontique::Blob;
use parley::{
    Alignment, AlignmentOptions, FontContext, FontFamily, FontWeight, LayoutContext, StyleProperty,
};

pub use render::GlyphCache;

/// The families `scripts/fetch-fonts.sh` installs, as their fonts name
/// themselves.
pub const SANS: &str = "Noto Sans";
pub const MONO: &str = "Noto Sans Mono";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weight {
    Regular,
    Bold,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Start,
    Center,
    End,
}

/// How to set a piece of text. `size` is the font size in pixels (the em);
/// the line is taller, by the font's own ascent + descent + gap.
#[derive(Clone, Copy, Debug)]
pub struct Style<'a> {
    pub family: &'a str,
    pub size: f32,
    pub weight: Weight,
    /// `0x00RRGGBB`.
    pub color: u32,
    pub align: Align,
}

impl<'a> Style<'a> {
    pub fn new(family: &'a str, size: f32) -> Style<'a> {
        Style { family, size, weight: Weight::Regular, color: 0xFF_FFFF, align: Align::Start }
    }

    pub fn bold(self) -> Self {
        Style { weight: Weight::Bold, ..self }
    }

    pub fn color(self, color: u32) -> Self {
        Style { color, ..self }
    }

    pub fn align(self, align: Align) -> Self {
        Style { align, ..self }
    }
}

/// Registered fonts plus `parley`'s scratch state. One per program.
pub struct Fonts {
    fcx: FontContext,
    lcx: LayoutContext<u32>,
}

impl Default for Fonts {
    fn default() -> Self {
        Self::new()
    }
}

impl Fonts {
    pub fn new() -> Fonts {
        Fonts { fcx: FontContext::new(), lcx: LayoutContext::new() }
    }

    /// Registers every face in a TTF/OTF/TTC file. Returns the family
    /// names it added faces to (a Bold file joins its Regular's family);
    /// empty if `bytes` is not a font.
    pub fn add(&mut self, bytes: Vec<u8>) -> Vec<String> {
        let added = self.fcx.collection.register_fonts(Blob::new(Arc::new(bytes)), None);
        let mut names = Vec::new();
        for (id, _) in added {
            if let Some(name) = self.fcx.collection.family_name(id) {
                names.push(name.to_string());
            }
        }
        names
    }

    /// Every registered family's name.
    pub fn families(&mut self) -> Vec<String> {
        self.fcx.collection.family_names().map(|n| n.to_string()).collect()
    }

    /// Lays out `text` in `style`, wrapped at `max_width` pixels if given
    /// (a word longer than the line is broken inside), `\n` forcing a
    /// break. Line boxes are aligned within `max_width`, or within the
    /// widest line without one.
    pub fn layout(&mut self, text: &str, style: &Style, max_width: Option<f32>) -> TextLayout {
        let mut b = self.lcx.ranged_builder(&mut self.fcx, text, 1.0, true);
        b.push_default(StyleProperty::FontFamily(FontFamily::named(style.family)));
        b.push_default(StyleProperty::FontSize(style.size));
        b.push_default(StyleProperty::FontWeight(match style.weight {
            Weight::Regular => FontWeight::NORMAL,
            Weight::Bold => FontWeight::BOLD,
        }));
        b.push_default(StyleProperty::Brush(style.color));
        b.push_default(StyleProperty::OverflowWrap(parley::OverflowWrap::Anywhere));
        let mut layout = b.build(text);
        layout.break_all_lines(max_width);
        let align = match style.align {
            Align::Start => Alignment::Start,
            Align::Center => Alignment::Center,
            Align::End => Alignment::End,
        };
        layout.align(align, AlignmentOptions::default());
        TextLayout { inner: layout }
    }

    /// The pixel size `(w, h)` a layout of `text` would take: the width of
    /// its widest line (trailing spaces excluded) and the sum of its line
    /// heights, rounded up.
    pub fn measure(&mut self, text: &str, style: &Style, max_width: Option<f32>) -> (i32, i32) {
        self.layout(text, style, max_width).size()
    }
}

/// One laid-out line, in pixels relative to the layout's top left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LineBox {
    /// Left edge of the line's content after alignment.
    pub x: f32,
    /// Width of the content, trailing spaces excluded.
    pub width: f32,
    pub top: f32,
    pub height: f32,
    pub baseline: f32,
    /// Byte range of the source text on this line.
    pub start: usize,
    pub end: usize,
}

/// Text shaped and broken into lines, ready to draw with a [`GlyphCache`].
pub struct TextLayout {
    inner: parley::Layout<u32>,
}

impl TextLayout {
    pub fn width(&self) -> f32 {
        self.inner.width()
    }

    pub fn height(&self) -> f32 {
        self.inner.height()
    }

    /// `(width, height)` rounded up to whole pixels.
    pub fn size(&self) -> (i32, i32) {
        (ceil(self.width()), ceil(self.height()))
    }

    pub fn lines(&self) -> impl Iterator<Item = LineBox> + '_ {
        self.inner.lines().map(|l| {
            let m = l.metrics();
            let r = l.text_range();
            LineBox {
                x: m.offset,
                width: m.advance - m.trailing_whitespace,
                top: m.block_min_coord,
                height: m.line_height,
                baseline: m.baseline,
                start: r.start,
                end: r.end,
            }
        })
    }

    /// Every glyph id, in visual order — `0` is `.notdef`, a character the
    /// font has no glyph for.
    pub fn glyph_ids(&self) -> Vec<u32> {
        let mut ids = Vec::new();
        for line in self.inner.lines() {
            for item in line.items() {
                if let parley::PositionedLayoutItem::GlyphRun(run) = item {
                    ids.extend(run.glyphs().map(|g| g.id));
                }
            }
        }
        ids
    }

    /// The underlying `parley` layout, for cursor and hit-testing work
    /// this API does not cover yet.
    pub fn parley(&self) -> &parley::Layout<u32> {
        &self.inner
    }
}

pub(crate) fn floor(x: f32) -> i32 {
    let i = x as i32;
    if (i as f32) > x {
        i - 1
    } else {
        i
    }
}

pub(crate) fn ceil(x: f32) -> i32 {
    let i = x as i32;
    if (i as f32) < x {
        i + 1
    } else {
        i
    }
}

#[cfg(test)]
mod tests {
    use super::{ceil, floor};

    #[test]
    fn floor_and_ceil_round_the_right_way() {
        assert_eq!((floor(1.5), ceil(1.5)), (1, 2));
        assert_eq!((floor(-1.5), ceil(-1.5)), (-2, -1));
        assert_eq!((floor(3.0), ceil(3.0)), (3, 3));
        assert_eq!((floor(-0.25), ceil(-0.25)), (-1, 0));
    }
}
