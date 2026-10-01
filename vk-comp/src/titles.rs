//! Window titles as pixels: each title rasterised once over its bar's colour with the text engine (proportional Noto from `/mnt/usr/share/fonts`,
//! or the bitmap fallback without it), redrawn only when the title, the focus or the bar's width changes. The renderer draws them as pixel sources.

use std::collections::BTreeMap;

use draw::Canvas;
use gui::compositor::{TITLE_FOCUSED, TITLE_UNFOCUSED};

use crate::text_util::{Style, Text, SANS};

pub struct TitleImg {
    title: String,
    focused: bool,
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
    next_version: u64,
}

impl Titles {
    pub fn new(scale: i32) -> Titles {
        Titles { text: Text::load(), cache: BTreeMap::new(), size: 13.0 * scale as f32, next_version: 1 }
    }

    pub fn missing_fonts(&self) -> usize {
        self.text.missing
    }

    /// The image of window `id`'s title in an `area_w x area_h` box (made, or kept if nothing changed).
    pub fn image(&mut self, id: u32, title: &str, focused: bool, area_w: i32, area_h: i32) -> &TitleImg {
        let stale = self.cache.get(&id).is_none_or(|c| c.title != title || c.focused != focused || (c.w, c.h) != (area_w, area_h));
        if stale {
            let bg = if focused { TITLE_FOCUSED } else { TITLE_UNFOCUSED };
            let fg = if focused { 0x00F0_F0F0 } else { 0x00B0_B0B8 };
            let mut px = vec![bg; (area_w.max(0) * area_h.max(0)) as usize];
            if area_w > 0 && area_h > 0 {
                let mut cv = Canvas::new(&mut px, area_w as usize, area_h as usize, area_w as usize);
                let st = Style::new(SANS, self.size).bold().color(fg);
                let (_, lh) = self.text.measure("Hg", &st, None);
                self.text.draw(&mut cv, title, &st, None, 0, (area_h - lh) / 2);
            }
            let version = self.next_version;
            self.next_version += 1;
            self.cache.insert(id, TitleImg { title: String::from(title), focused, w: area_w, h: area_h, px, version });
        }
        &self.cache[&id]
    }

    /// Forgets the titles of windows that are gone.
    pub fn retain(&mut self, live: impl Fn(u32) -> bool) {
        self.cache.retain(|id, _| live(*id));
    }
}
