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
            let px = gui::theme::text_pixels(&cov, w, h, fg, shadow, self.scale as usize);
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
