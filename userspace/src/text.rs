//! Text for graphical programs: the `text` crate (proportional Noto at any
//! size, `docs/gui/text-plan.md`) with its fonts read from disk.
//!
//! [`Text::load`] reads [`FONT_FILES`] from [`FONT_DIR`] into the heap
//! (~1.6 MB; every program has its own copy — there is no file `mmap` to
//! share them yet). Without fonts — no `/mnt`, or a disk synced before
//! they existed — the program still runs: [`Text::draw`] and
//! [`Text::measure`] fall back to `draw`'s bitmap Noto Sans Mono at the
//! nearest of its four sizes, one line per `\n`, no wrapping, Basic Latin
//! only. [`Text::fonts`] says which one is live.
//!
//! Linking this pulls in `parley` + `swash` (~1.2 MB), so programs that use
//! it belong on the disk (`/mnt/bin`), not embedded in the kernel.

use alloc::vec::Vec;

use draw::smooth::{FontWeight, RasterHeight, Smooth};
use draw::Canvas;
pub use text::cache::Stats;
pub use text::{Align, Fonts, GlyphCache, LineBox, Style, TextLayout, Weight, MONO, SANS};

use crate::syscall;

pub const FONT_DIR: &str = "/mnt/usr/share/fonts";
pub const FONT_FILES: [&str; 4] =
    ["NotoSans-Regular.ttf", "NotoSans-Bold.ttf", "NotoSansMono-Regular.ttf", "NotoSansMono-Bold.ttf"];

/// Registered fonts and the glyph cache, or neither (bitmap fallback).
pub struct Text {
    ttf: Option<(Fonts, GlyphCache)>,
    /// Font files that could not be read or registered, for a log line.
    pub missing: usize,
}

impl Text {
    /// Loads every font it can find. Never fails: with none, the bitmap
    /// fallback is used.
    pub fn load() -> Text {
        let mut fonts = Fonts::new();
        let mut found = 0;
        let mut path = alloc::string::String::new();
        for name in FONT_FILES {
            path.clear();
            path.push_str(FONT_DIR);
            path.push('/');
            path.push_str(name);
            if let Ok(bytes) = read_file(&path) {
                if !fonts.add(bytes).is_empty() {
                    found += 1;
                }
            }
        }
        let missing = FONT_FILES.len() - found;
        let ttf = (found > 0).then(|| (fonts, GlyphCache::default()));
        Text { ttf, missing }
    }

    /// Whether TrueType fonts are live (false: the bitmap fallback).
    pub fn fonts(&self) -> bool {
        self.ttf.is_some()
    }

    /// Glyph cache counters (zero with the fallback).
    pub fn stats(&self) -> Stats {
        self.ttf.as_ref().map(|(_, c)| c.stats()).unwrap_or_default()
    }

    /// The layout, for programs that need line boxes or glyphs; `None`
    /// with the fallback.
    pub fn layout(&mut self, s: &str, style: &Style, max_width: Option<f32>) -> Option<TextLayout> {
        self.ttf.as_mut().map(|(f, _)| f.layout(s, style, max_width))
    }

    /// Pixel size `(w, h)` `s` takes in `style`, wrapped at `max_width`.
    pub fn measure(&mut self, s: &str, style: &Style, max_width: Option<f32>) -> (i32, i32) {
        match self.ttf.as_mut() {
            Some((f, _)) => f.measure(s, style, max_width),
            None => {
                let font = fallback_font(style);
                let lines = fallback_lines(s, font, max_width);
                let w = lines.iter().map(|l| font.width(l)).max().unwrap_or(0);
                (w, lines.len() as i32 * font.cell().1)
            }
        }
    }

    /// Draws `s` with its box's top left at `(x, y)`, blended over the
    /// canvas; returns the box's size, as [`measure`](Self::measure).
    pub fn draw(&mut self, cv: &mut Canvas, s: &str, style: &Style, max_width: Option<f32>, x: i32, y: i32) -> (i32, i32) {
        match self.ttf.as_mut() {
            Some((f, c)) => {
                let l = f.layout(s, style, max_width);
                c.draw(cv, &l, x, y);
                l.size()
            }
            None => {
                let font = fallback_font(style);
                let (_, ch) = font.cell();
                let lines = fallback_lines(s, font, max_width);
                let w = lines.iter().map(|l| font.width(l)).max().unwrap_or(0);
                let boxw = max_width.map(|m| m as i32).unwrap_or(w);
                for (i, line) in lines.iter().enumerate() {
                    let lx = match style.align {
                        Align::Start => x,
                        Align::Center => x + (boxw - font.width(line)) / 2,
                        Align::End => x + boxw - font.width(line),
                    };
                    cv.smooth_text(font, lx, y + i as i32 * ch, line, style.color);
                }
                (w, lines.len() as i32 * ch)
            }
        }
    }

    /// Draws a layout from [`layout`](Self::layout); nothing with the
    /// fallback (which never hands one out).
    pub fn draw_layout(&mut self, cv: &mut Canvas, l: &TextLayout, x: i32, y: i32) {
        if let Some((_, c)) = self.ttf.as_mut() {
            c.draw(cv, l, x, y);
        }
    }
}

/// The bitmap size closest to `style.size`: 16, 20, 24 or 32 px.
fn fallback_font(style: &Style) -> Smooth {
    let size = match style.size as i32 {
        ..=17 => RasterHeight::Size16,
        18..=21 => RasterHeight::Size20,
        22..=27 => RasterHeight::Size24,
        _ => RasterHeight::Size32,
    };
    let weight = match style.weight {
        Weight::Regular => FontWeight::Regular,
        Weight::Bold => FontWeight::Bold,
    };
    Smooth::new(size, weight)
}

/// `s` cut into lines for the monospaced fallback: at every `\n`, and
/// greedily at the last space that fits `max_width` (a word longer than
/// the line is cut inside it), the space dropped — what `parley` does, in
/// cells.
fn fallback_lines(s: &str, font: Smooth, max_width: Option<f32>) -> Vec<&str> {
    let cols = max_width.map(|m| ((m as i32) / font.cell().0).max(1) as usize);
    let mut out = Vec::new();
    for mut line in s.split('\n') {
        let Some(cols) = cols else {
            out.push(line);
            continue;
        };
        loop {
            // Byte offset just past `cols` characters, if the line is longer.
            let Some((cut, _)) = line.char_indices().nth(cols) else {
                out.push(line);
                break;
            };
            // A space right at the limit breaks there; `cut` is a char
            // boundary, `cut + 1` might not be.
            if line[cut..].starts_with(' ') {
                out.push(&line[..cut]);
                line = &line[cut + 1..];
                continue;
            }
            match line[..cut].rfind(' ') {
                Some(sp) if sp > 0 => {
                    out.push(&line[..sp]);
                    line = &line[sp + 1..];
                }
                _ => {
                    out.push(&line[..cut]);
                    line = &line[cut..];
                }
            }
        }
    }
    out
}

/// The whole of `path`, or the negative errno of the `open`.
pub fn read_file(path: &str) -> Result<Vec<u8>, i64> {
    let fd = syscall::with_cstr(path, |p| syscall::open(p, syscall::O_RDONLY));
    if fd < 0 {
        return Err(fd);
    }
    let fd = fd as i32;
    let mut buf = Vec::new();
    if let Ok(st) = syscall::fstat(fd) {
        buf.reserve_exact(st.st_size as usize);
    }
    let mut chunk = [0u8; 16384];
    loop {
        let n = syscall::read(fd, &mut chunk);
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
    }
    syscall::close(fd);
    Ok(buf)
}
