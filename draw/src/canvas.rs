//! [`Canvas`]: a clipped view of a pixel buffer and its primitives.
//!
//! Pixel `(x, y)` lives at `px[y * stride + x]`; `stride >= width`, so a
//! canvas can sit on a framebuffer whose rows are padded, and nothing here
//! ever writes the padding. Every primitive clips: any coordinates, even
//! far outside or negative, are safe and draw only what falls inside.
//!
//! Integer pixel coordinates for the axis-aligned primitives; fixed point
//! ([`FP`](crate::FP) per pixel) for [`disc`](Canvas::disc) and
//! [`glow`](Canvas::glow), so moving things glide instead of snapping to
//! whole pixels.

use crate::color::{add, mix, scale};
use crate::FP;

pub struct Canvas<'a> {
    px: &'a mut [u32],
    w: i32,
    h: i32,
    stride: usize,
}

impl<'a> Canvas<'a> {
    /// A `w x h` view of `px` with rows `stride` pixels apart.
    ///
    /// Panics if `stride < w` or `px` is too short for the last row.
    pub fn new(px: &'a mut [u32], w: usize, h: usize, stride: usize) -> Canvas<'a> {
        assert!(stride >= w, "stride {} < width {}", stride, w);
        if h > 0 {
            assert!(px.len() >= (h - 1) * stride + w, "buffer too short for {}x{} (stride {})", w, h, stride);
        }
        Canvas { px, w: w as i32, h: h as i32, stride }
    }

    pub fn width(&self) -> i32 {
        self.w
    }

    pub fn height(&self) -> i32 {
        self.h
    }

    fn at(&self, x: i32, y: i32) -> usize {
        y as usize * self.stride + x as usize
    }

    fn inside(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && x < self.w && y < self.h
    }

    /// The pixel at `(x, y)`, or `None` outside the canvas.
    pub fn get(&self, x: i32, y: i32) -> Option<u32> {
        self.inside(x, y).then(|| self.px[self.at(x, y)])
    }

    pub fn put(&mut self, x: i32, y: i32, c: u32) {
        if self.inside(x, y) {
            let i = self.at(x, y);
            self.px[i] = c;
        }
    }

    /// Adds light to one pixel (see [`add`]).
    pub fn add_px(&mut self, x: i32, y: i32, c: u32) {
        if self.inside(x, y) {
            let i = self.at(x, y);
            self.px[i] = add(self.px[i], c);
        }
    }

    /// Fills the whole canvas.
    pub fn fill(&mut self, c: u32) {
        self.rect(0, 0, self.w, self.h, c);
    }

    /// A filled rectangle.
    pub fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, c: u32) {
        let (x0, x1) = (x.max(0), x.saturating_add(w).min(self.w));
        let (y0, y1) = (y.max(0), y.saturating_add(h).min(self.h));
        if x0 >= x1 {
            return;
        }
        for yy in y0..y1 {
            let row = self.at(x0, yy);
            self.px[row..row + (x1 - x0) as usize].fill(c);
        }
    }

    /// A horizontal line of `len` pixels.
    pub fn hline(&mut self, x: i32, y: i32, len: i32, c: u32) {
        self.rect(x, y, len, 1, c);
    }

    /// A vertical line of `len` pixels.
    pub fn vline(&mut self, x: i32, y: i32, len: i32, c: u32) {
        self.rect(x, y, 1, len, c);
    }

    /// A one-pixel rectangle outline.
    pub fn frame(&mut self, x: i32, y: i32, w: i32, h: i32, c: u32) {
        if w <= 0 || h <= 0 {
            return;
        }
        self.hline(x, y, w, c);
        self.hline(x, y + h - 1, w, c);
        self.vline(x, y, h, c);
        self.vline(x + w - 1, y, h, c);
    }

    /// Scales every pixel by `k`/256 — darkening under an overlay.
    pub fn dim(&mut self, k: i32) {
        for y in 0..self.h {
            let row = self.at(0, y);
            for p in &mut self.px[row..row + self.w as usize] {
                *p = scale(*p, k);
            }
        }
    }

    /// Copies a `sw x sh` image (rows `sw` apart) with its top left at
    /// `(dx, dy)`, clipped.
    pub fn blit(&mut self, src: &[u32], sw: usize, sh: usize, dx: i32, dy: i32) {
        let (sw, sh) = (sw as i32, sh as i32);
        let x0 = dx.max(0);
        let x1 = (dx + sw).min(self.w);
        if x0 >= x1 {
            return;
        }
        for y in dy.max(0)..(dy + sh).min(self.h) {
            let s = ((y - dy) * sw + (x0 - dx)) as usize;
            let d = self.at(x0, y);
            let n = (x1 - x0) as usize;
            self.px[d..d + n].copy_from_slice(&src[s..s + n]);
        }
    }

    /// Pixel bounds of a fixed-point circle, clipped; `None` if nothing of
    /// it is on the canvas.
    fn circle_box(&self, cx: i32, cy: i32, r: i32) -> Option<(i32, i32, i32, i32)> {
        let x0 = (cx - r).div_euclid(FP).max(0);
        let x1 = (cx + r).div_euclid(FP).min(self.w - 1);
        let y0 = (cy - r).div_euclid(FP).max(0);
        let y1 = (cy + r).div_euclid(FP).min(self.h - 1);
        (r > 0 && x0 <= x1 && y0 <= y1).then_some((x0, x1, y0, y1))
    }

    /// An antialiased disc: centre `(cx, cy)` and radius `r` in fixed
    /// point. With `shade`, darker towards the rim and lit from the top
    /// left, so it reads as a sphere — and a row of them as a tube.
    pub fn disc(&mut self, cx: i32, cy: i32, r: i32, c: u32, shade: bool) {
        let Some((x0, x1, y0, y1)) = self.circle_box(cx, cy, r) else { return };
        let r2 = r as i64 * r as i64;
        for y in y0..=y1 {
            let dy = (y * FP + FP / 2 - cy) as i64;
            for x in x0..=x1 {
                let dx = (x * FP + FP / 2 - cx) as i64;
                let d2 = dx * dx + dy * dy;
                if d2 >= r2 {
                    continue;
                }
                // r - d ≈ (r² - d²) / 2r: how much of the rim pixel is in.
                let cov = ((r2 - d2) / (2 * r as i64)).min(FP as i64) as i32;
                let col = if shade {
                    let rim = (d2 * 110 / r2) as i32;
                    let light = ((-dx - dy) * 60 / (2 * r as i64)) as i32;
                    scale(c, 256 - rim + light.max(0))
                } else {
                    c
                };
                let i = self.at(x, y);
                self.px[i] = if cov >= FP { col } else { mix(self.px[i], col, cov) };
            }
        }
    }

    /// Additive light around `(cx, cy)` out to radius `r` (fixed point),
    /// falling off quadratically; `strength` 256 is `c` at full at the
    /// centre. Never darkens.
    pub fn glow(&mut self, cx: i32, cy: i32, r: i32, c: u32, strength: i32) {
        let Some((x0, x1, y0, y1)) = self.circle_box(cx, cy, r) else { return };
        let r2 = r as i64 * r as i64;
        for y in y0..=y1 {
            let dy = (y * FP + FP / 2 - cy) as i64;
            for x in x0..=x1 {
                let dx = (x * FP + FP / 2 - cx) as i64;
                let d2 = dx * dx + dy * dy;
                if d2 >= r2 {
                    continue;
                }
                let f = ((r2 - d2) * 256 / r2) as i32;
                let k = f * f / 256 * strength / 256;
                let i = self.at(x, y);
                self.px[i] = add(self.px[i], scale(c, k));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAD: u32 = 0xAAAAAA;

    /// A 16x10 canvas with stride 20 over a buffer pre-filled with `PAD`,
    /// so writes into the padding columns are detectable.
    fn buf() -> [u32; 20 * 10] {
        [PAD; 20 * 10]
    }

    fn padding_untouched(px: &[u32]) -> bool {
        (0..10).all(|y| px[y * 20 + 16..y * 20 + 20].iter().all(|&p| p == PAD))
    }

    #[test]
    fn rect_clips_and_respects_stride() {
        let mut px = buf();
        let mut cv = Canvas::new(&mut px, 16, 10, 20);
        cv.rect(-5, -5, 100, 100, 0x112233);
        assert_eq!(cv.get(0, 0), Some(0x112233));
        assert_eq!(cv.get(15, 9), Some(0x112233));
        assert_eq!(cv.get(16, 0), None);
        assert!(padding_untouched(&px));
    }

    #[test]
    fn rect_draws_exactly_its_pixels() {
        let mut px = buf();
        let mut cv = Canvas::new(&mut px, 16, 10, 20);
        cv.fill(0);
        cv.rect(2, 3, 4, 2, 1);
        for y in 0..10 {
            for x in 0..16 {
                let inside = (2..6).contains(&x) && (3..5).contains(&y);
                assert_eq!(cv.get(x, y), Some(inside as u32), "({}, {})", x, y);
            }
        }
    }

    #[test]
    fn degenerate_and_far_away_rects_draw_nothing() {
        let mut px = buf();
        let mut cv = Canvas::new(&mut px, 16, 10, 20);
        cv.fill(0);
        cv.rect(3, 3, 0, 5, 9);
        cv.rect(3, 3, -4, 5, 9);
        cv.rect(i32::MAX - 1, 0, 10, 10, 9);
        cv.rect(i32::MIN, i32::MIN, 5, 5, 9);
        assert!((0..10).all(|y| (0..16).all(|x| cv.get(x, y) == Some(0))));
    }

    #[test]
    fn frame_is_only_the_border() {
        let mut px = buf();
        let mut cv = Canvas::new(&mut px, 16, 10, 20);
        cv.fill(0);
        cv.frame(1, 1, 5, 4, 7);
        assert_eq!(cv.get(1, 1), Some(7));
        assert_eq!(cv.get(5, 4), Some(7));
        assert_eq!(cv.get(3, 2), Some(0));
        assert_eq!(cv.get(0, 0), Some(0));
    }

    #[test]
    fn put_and_add_ignore_outside() {
        let mut px = buf();
        let mut cv = Canvas::new(&mut px, 16, 10, 20);
        cv.fill(0x101010);
        cv.put(-1, 0, 5);
        cv.put(16, 0, 5);
        cv.add_px(3, 3, 0x010203);
        assert_eq!(cv.get(3, 3), Some(0x111213));
        assert!(padding_untouched(&px));
    }

    #[test]
    fn disc_covers_its_centre_and_nothing_far() {
        let mut px = buf();
        let mut cv = Canvas::new(&mut px, 16, 10, 20);
        cv.fill(0);
        cv.disc(8 * FP, 5 * FP, 3 * FP, 0xFFFFFF, false);
        assert_eq!(cv.get(8, 5), Some(0xFFFFFF));
        assert_eq!(cv.get(7, 4), Some(0xFFFFFF));
        assert_eq!(cv.get(0, 0), Some(0));
        assert_eq!(cv.get(12, 5), Some(0)); // 3.5 px from the centre
        assert!(padding_untouched(&px));
    }

    #[test]
    fn disc_is_symmetric() {
        let mut px = [0u32; 16 * 16];
        let mut cv = Canvas::new(&mut px, 16, 16, 16);
        cv.disc(8 * FP, 8 * FP, 5 * FP, 0xFFFFFF, false);
        for y in 0..16 {
            for x in 0..16 {
                assert_eq!(cv.get(x, y), cv.get(15 - x, y), "({}, {})", x, y);
                assert_eq!(cv.get(x, y), cv.get(x, 15 - y), "({}, {})", x, y);
            }
        }
    }

    #[test]
    fn disc_rim_is_antialiased() {
        let mut px = [0u32; 16 * 16];
        let mut cv = Canvas::new(&mut px, 16, 16, 16);
        cv.disc(8 * FP, 8 * FP, 5 * FP, 0xFFFFFF, false);
        let partial = (0..16)
            .flat_map(|y| (0..16).map(move |x| (x, y)))
            .filter(|&(x, y)| !matches!(cv.get(x, y), Some(0) | Some(0xFFFFFF)))
            .count();
        assert!(partial > 0, "no blended rim pixels");
    }

    #[test]
    fn shaded_disc_is_brighter_top_left_than_bottom_right() {
        let mut px = [0u32; 16 * 16];
        let mut cv = Canvas::new(&mut px, 16, 16, 16);
        cv.disc(8 * FP, 8 * FP, 6 * FP, 0x808080, true);
        let tl = cv.get(5, 5).unwrap() & 0xFF;
        let br = cv.get(10, 10).unwrap() & 0xFF;
        assert!(tl > br, "{} <= {}", tl, br);
    }

    #[test]
    fn discs_off_the_edge_are_clipped_not_panicking() {
        let mut px = buf();
        let mut cv = Canvas::new(&mut px, 16, 10, 20);
        cv.fill(0);
        cv.disc(-2 * FP, -2 * FP, 4 * FP, 0xFFFFFF, true);
        cv.disc(17 * FP, 11 * FP, 4 * FP, 0xFFFFFF, true);
        cv.disc(-100 * FP, 5 * FP, 3 * FP, 0xFFFFFF, true);
        cv.disc(5 * FP, 5 * FP, 0, 0xFFFFFF, true);
        cv.glow(-3 * FP, 20 * FP, 6 * FP, 0xFFFFFF, 256);
        assert_ne!(cv.get(0, 0), Some(0));
        assert!(padding_untouched(&px));
    }

    #[test]
    fn glow_only_adds_light() {
        let mut px = [0u32; 16 * 16];
        for (i, p) in px.iter_mut().enumerate() {
            *p = (i as u32 * 0x010101) & 0x3F3F3F;
        }
        let before = px;
        let mut cv = Canvas::new(&mut px, 16, 16, 16);
        cv.glow(8 * FP, 8 * FP, 6 * FP, 0x4080FF, 200);
        for (a, b) in before.iter().zip(px.iter()) {
            let (ar, ag, ab) = crate::color::channels(*a);
            let (br, bg, bb) = crate::color::channels(*b);
            assert!(br >= ar && bg >= ag && bb >= ab);
        }
        assert_ne!(before, px);
    }

    #[test]
    fn glow_with_zero_strength_changes_nothing() {
        let mut px = [0x202020u32; 16 * 16];
        let mut cv = Canvas::new(&mut px, 16, 16, 16);
        cv.glow(8 * FP, 8 * FP, 6 * FP, 0xFFFFFF, 0);
        assert!(px.iter().all(|&p| p == 0x202020));
    }

    #[test]
    fn dim_scales_only_the_visible_area() {
        let mut px = buf();
        let mut cv = Canvas::new(&mut px, 16, 10, 20);
        cv.fill(0x808080);
        cv.dim(128);
        assert_eq!(cv.get(4, 4), Some(0x404040));
        assert!(padding_untouched(&px));
    }

    #[test]
    fn blit_copies_and_clips() {
        let src: [u32; 3 * 2] = [1, 2, 3, 4, 5, 6];
        let mut px = buf();
        let mut cv = Canvas::new(&mut px, 16, 10, 20);
        cv.fill(0);
        cv.blit(&src, 3, 2, 1, 1);
        assert_eq!(cv.get(1, 1), Some(1));
        assert_eq!(cv.get(3, 2), Some(6));
        cv.blit(&src, 3, 2, -1, -1); // only the bottom-right 2x1 lands
        assert_eq!(cv.get(0, 0), Some(5));
        assert_eq!(cv.get(1, 0), Some(6));
        cv.blit(&src, 3, 2, 15, 9); // only the top-left pixel lands
        assert_eq!(cv.get(15, 9), Some(1));
        cv.blit(&src, 3, 2, 40, 40);
        assert!(padding_untouched(&px));
    }

    #[test]
    #[should_panic(expected = "stride")]
    fn stride_below_width_is_refused() {
        let mut px = [0u32; 100];
        Canvas::new(&mut px, 10, 5, 8);
    }

    #[test]
    #[should_panic(expected = "too short")]
    fn short_buffer_is_refused() {
        let mut px = [0u32; 30];
        Canvas::new(&mut px, 10, 5, 10);
    }
}
