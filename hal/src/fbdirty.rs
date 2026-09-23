//! `DirtyRect` — the part of a framebuffer's RAM shadow that has not yet
//! been copied to VRAM.
//!
//! The kernel's framebuffer console draws into a shadow in ordinary
//! write-back RAM and copies the changed region to VRAM afterwards,
//! because on the physical machine this kernel is brought up on, reading
//! VRAM costs ~4 MB/s: one scroll, which reads the whole screen back,
//! measured 2.18 s (`docs/fb/wc-shadow-plan.md`). With the shadow, VRAM is
//! only ever written, and this type says which part to write.
//!
//! A single bounding rectangle, not a list or a row bitset. The console
//! dirties one contiguous region almost always: a cursor cell, the
//! current line, or the whole screen after a scroll. A rectangle rather
//! than a row range because the blinking cursor dirties 8x9 pixels, and
//! flushing its nine full rows instead would be ~69 KB of uncached writes
//! per blink at 1920 px. Two far-apart marks do over-flush the space
//! between them. That is the trade, and `fb_flush` in `/proc/fbinfo` is
//! where it would show.

/// Half-open rectangle `[x0, x1) x [y0, y1)` in pixels, or empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
}

impl Rect {
    pub fn width(&self) -> usize {
        self.x1 - self.x0
    }
    pub fn height(&self) -> usize {
        self.y1 - self.y0
    }
}

/// Accumulates marked rectangles, clipped to the screen, until `take`.
#[derive(Clone, Copy, Debug)]
pub struct DirtyRect {
    screen_w: usize,
    screen_h: usize,
    pending: Option<Rect>,
}

impl DirtyRect {
    pub const fn new(screen_w: usize, screen_h: usize) -> Self {
        Self { screen_w, screen_h, pending: None }
    }

    /// Add `[x, x+w) x [y, y+h)`. Clipped to the screen, so a caller can
    /// pass a rectangle that runs off an edge, as `fill_rect` accepts one.
    /// A rectangle that is empty after clipping changes nothing.
    pub fn mark(&mut self, x: usize, y: usize, w: usize, h: usize) {
        let x0 = x.min(self.screen_w);
        let y0 = y.min(self.screen_h);
        let x1 = x.saturating_add(w).min(self.screen_w);
        let y1 = y.saturating_add(h).min(self.screen_h);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        self.pending = Some(match self.pending {
            None => Rect { x0, y0, x1, y1 },
            Some(r) => Rect {
                x0: r.x0.min(x0),
                y0: r.y0.min(y0),
                x1: r.x1.max(x1),
                y1: r.y1.max(y1),
            },
        });
    }

    /// Mark the whole screen, which is what a scroll does.
    pub fn mark_all(&mut self) {
        self.mark(0, 0, self.screen_w, self.screen_h);
    }

    /// The accumulated rectangle, leaving nothing pending.
    pub fn take(&mut self) -> Option<Rect> {
        self.pending.take()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_marked_takes_nothing() {
        let mut d = DirtyRect::new(100, 50);
        assert!(d.is_empty());
        assert_eq!(d.take(), None);
    }

    #[test]
    fn one_mark_comes_back_exactly_and_take_empties_it() {
        let mut d = DirtyRect::new(100, 50);
        d.mark(8, 9, 8, 9);
        assert_eq!(d.take(), Some(Rect { x0: 8, y0: 9, x1: 16, y1: 18 }));
        assert_eq!(d.take(), None, "take leaves nothing pending");
    }

    #[test]
    fn two_marks_become_their_bounding_rectangle() {
        let mut d = DirtyRect::new(100, 50);
        d.mark(10, 10, 5, 5);
        d.mark(2, 20, 3, 1);
        assert_eq!(d.take(), Some(Rect { x0: 2, y0: 10, x1: 15, y1: 21 }));
    }

    #[test]
    fn marks_are_clipped_to_the_screen() {
        let mut d = DirtyRect::new(100, 50);
        // `ESC[J` on the bottom row passes a rectangle like this.
        d.mark(98, 48, 999, 999);
        assert_eq!(d.take(), Some(Rect { x0: 98, y0: 48, x1: 100, y1: 50 }));
    }

    #[test]
    fn a_mark_entirely_off_screen_or_empty_changes_nothing() {
        let mut d = DirtyRect::new(100, 50);
        d.mark(100, 0, 5, 5);
        d.mark(0, 50, 5, 5);
        d.mark(3, 3, 0, 5);
        assert_eq!(d.take(), None);
        d.mark(1, 1, 1, 1);
        d.mark(200, 200, 5, 5);
        assert_eq!(d.take(), Some(Rect { x0: 1, y0: 1, x1: 2, y1: 2 }));
    }

    #[test]
    fn overflowing_coordinates_saturate_instead_of_wrapping() {
        let mut d = DirtyRect::new(100, 50);
        d.mark(10, 10, usize::MAX, usize::MAX);
        assert_eq!(d.take(), Some(Rect { x0: 10, y0: 10, x1: 100, y1: 50 }));
    }

    #[test]
    fn mark_all_is_the_whole_screen() {
        let mut d = DirtyRect::new(100, 50);
        d.mark(5, 5, 1, 1);
        d.mark_all();
        let r = d.take().unwrap();
        assert_eq!(r, Rect { x0: 0, y0: 0, x1: 100, y1: 50 });
        assert_eq!((r.width(), r.height()), (100, 50));
    }
}
