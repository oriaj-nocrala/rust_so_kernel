//! Window titles as pixels: each title rasterised once with the text engine (proportional Noto from `/mnt/usr/share/fonts`, or the bitmap
//! fallback without it), redrawn only when the title, its colours or the bar's width changes. The image is premultiplied ARGB, transparent
//! around the glyphs, so the bar's look (a gradient in most themes) shows through; the renderer draws it "over" (`CR_PREMUL`).

use std::collections::BTreeMap;

use draw::Canvas;

use crate::text_util::{Style, Text, SANS};

pub struct TitleImg {
    title: String,
    fg: u32,
    shadow: u32,
    pub w: i32,
    pub h: i32,
    pub px: Vec<u32>,
    /// Moves whenever `px` does: the renderer uploads when it sees a new one.
    pub version: u64,
}

pub struct Titles {
    text: Text,
    cache: BTreeMap<u32, TitleImg>,
    size: f32,
    scale: i32,
    next_version: u64,
}

impl Titles {
    pub fn new(scale: i32) -> Titles {
        Titles { text: Text::load(), cache: BTreeMap::new(), size: 13.0 * scale as f32, scale, next_version: 1 }
    }

    pub fn missing_fonts(&self) -> usize {
        self.text.missing
    }

    /// The image of window `id`'s title in an `area_w x area_h` box, in `fg` (`0x00RRGGBB`) over a shadow `shadow` (`0xAARRGGBB`, alpha 0 =
    /// none) one pixel × scale down and right (made, or kept if nothing changed).
    pub fn image(&mut self, id: u32, title: &str, fg: u32, shadow: u32, area_w: i32, area_h: i32) -> &TitleImg {
        let stale = self.cache.get(&id).is_none_or(|c| c.title != title || c.fg != fg || c.shadow != shadow || (c.w, c.h) != (area_w, area_h));
        if stale {
            let (w, h) = (area_w.max(0) as usize, area_h.max(0) as usize);
            // the glyphs' coverage: white drawn on black
            let mut cov = vec![0u32; w * h];
            if w > 0 && h > 0 {
                let mut cv = Canvas::new(&mut cov, w, h, w);
                let st = Style::new(SANS, self.size).bold().color(0x00FF_FFFF);
                let (_, lh) = self.text.measure("Hg", &st, None);
                self.text.draw(&mut cv, title, &st, None, 0, (area_h - lh) / 2);
            }
            let px = premultiplied(&cov, w, h, fg, shadow, self.scale as usize);
            let version = self.next_version;
            self.next_version += 1;
            self.cache.insert(id, TitleImg { title: String::from(title), fg, shadow, w: area_w, h: area_h, px, version });
        }
        &self.cache[&id]
    }

    /// Forgets the titles of windows that are gone.
    pub fn retain(&mut self, live: impl Fn(u32) -> bool) {
        self.cache.retain(|id, _| live(*id));
    }
}

/// Text of coverage `cov` (white on black, `w x h`) in `fg`, over its shadow (`shadow`, offset by `off` pixels), as premultiplied ARGB.
fn premultiplied(cov: &[u32], w: usize, h: usize, fg: u32, shadow: u32, off: usize) -> Vec<u32> {
    let chan = |c: u32, sh: u32| (c >> sh) & 255;
    let sa = shadow >> 24;
    let mut out = vec![0u32; w * h];
    for y in 0..h {
        for x in 0..w {
            let t = cov[y * w + x] >> 8 & 255; // green: the coverage
            let s = if sa > 0 && x >= off && y >= off { (cov[(y - off) * w + x - off] >> 8 & 255) * sa / 255 } else { 0 };
            // text over shadow, premultiplied: a = t + s (1 - t)
            let a = t + s * (255 - t) / 255;
            let mut p = a << 24;
            for sh in [16, 8, 0] {
                let c = chan(fg, sh) * t / 255 + chan(shadow, sh) * s / 255 * (255 - t) / 255;
                p |= c.min(a) << sh;
            }
            out[y * w + x] = p;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::premultiplied;

    #[test]
    fn text_over_its_shadow_premultiplied() {
        // one row: full coverage, nothing, half coverage; the shadow is one pixel right of each
        let cov = [0x00FF_FFFF, 0, 0x0080_8080, 0];
        let px = premultiplied(&cov, 4, 1, 0x0020_4080, 0x8000_0000, 0);
        assert_eq!(px[0], 0xFF20_4080, "full coverage: the colour, opaque");
        assert_eq!(px[1], 0, "no glyph, no shadow: transparent");
        assert_eq!(px[2] >> 24, 0x80 + (0x80 * 0x80 / 255) * (255 - 0x80) / 255, "half a glyph over a shadow (offset 0: under itself)");
        let px = premultiplied(&cov, 4, 1, 0x00FF_FFFF, 0x8000_0000, 1);
        // offset 1 needs y >= 1 too: on one row there is no shadow
        assert_eq!(px[1], 0);
        let cov2 = [0x00FF_FFFF, 0, 0, 0];
        let px = premultiplied(&cov2, 2, 2, 0x00FF_FFFF, 0x8000_0000, 1);
        assert_eq!(px[3], 0x8000_0000, "the shadow, down and right of the glyph: black at half alpha");
        assert!(px.iter().all(|p| [16, 8, 0].iter().all(|s| (p >> s & 255) <= p >> 24)), "premultiplied: no channel above alpha");
    }
}
