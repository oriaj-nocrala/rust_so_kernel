//! Rectangles and regions (disjoint sets of rectangles).
//!
//! A [`Region`] never holds two overlapping rectangles, so its area is the
//! sum of theirs and composing each one touches every pixel once. Adding
//! a rectangle adds only the parts not already covered; subtracting one
//! splits each rectangle it hits into at most four.

use alloc::vec::Vec;

/// A rectangle of `w x h` pixels at `(x, y)`. Empty when either side is
/// `<= 0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Rect { x, y, w, h }
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    pub fn right(&self) -> i32 {
        self.x.saturating_add(self.w)
    }

    pub fn bottom(&self) -> i32 {
        self.y.saturating_add(self.h)
    }

    pub fn area(&self) -> i64 {
        if self.is_empty() { 0 } else { self.w as i64 * self.h as i64 }
    }

    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && py >= self.y && px < self.right() && py < self.bottom()
    }

    /// The overlap, or `None`.
    pub fn intersect(&self, o: &Rect) -> Option<Rect> {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = self.right().min(o.right());
        let y1 = self.bottom().min(o.bottom());
        if x1 > x0 && y1 > y0 {
            Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
        } else {
            None
        }
    }

    /// Smallest rectangle holding both (an empty one is ignored).
    pub fn union_box(&self, o: &Rect) -> Rect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        Rect::new(x0, y0, self.right().max(o.right()) - x0, self.bottom().max(o.bottom()) - y0)
    }

    pub fn translate(&self, dx: i32, dy: i32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    /// `self` minus `cut`, as up to four disjoint pieces: full-width bands
    /// above and below the overlap, then the parts left and right of it.
    pub fn subtract(&self, cut: &Rect, out: &mut Vec<Rect>) {
        let Some(i) = self.intersect(cut) else {
            if !self.is_empty() {
                out.push(*self);
            }
            return;
        };
        if i.y > self.y {
            out.push(Rect::new(self.x, self.y, self.w, i.y - self.y));
        }
        if i.bottom() < self.bottom() {
            out.push(Rect::new(self.x, i.bottom(), self.w, self.bottom() - i.bottom()));
        }
        if i.x > self.x {
            out.push(Rect::new(self.x, i.y, i.x - self.x, i.h));
        }
        if i.right() < self.right() {
            out.push(Rect::new(i.right(), i.y, self.right() - i.right(), i.h));
        }
    }
}

/// A set of pixels, as disjoint rectangles.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Region {
    rects: Vec<Rect>,
}

impl Region {
    pub const fn new() -> Self {
        Region { rects: Vec::new() }
    }

    pub fn from_rect(r: Rect) -> Self {
        let mut g = Region::new();
        g.add(r);
        g
    }

    pub fn rects(&self) -> &[Rect] {
        &self.rects
    }

    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    pub fn clear(&mut self) {
        self.rects.clear();
    }

    pub fn area(&self) -> i64 {
        self.rects.iter().map(Rect::area).sum()
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        self.rects.iter().any(|r| r.contains(x, y))
    }

    /// Adds the parts of `r` not already in the region.
    pub fn add(&mut self, r: Rect) {
        if r.is_empty() {
            return;
        }
        let mut pieces = alloc::vec![r];
        for have in &self.rects {
            let mut next = Vec::new();
            for p in &pieces {
                p.subtract(have, &mut next);
            }
            pieces = next;
            if pieces.is_empty() {
                return;
            }
        }
        self.rects.extend(pieces);
    }

    pub fn add_region(&mut self, o: &Region) {
        for r in &o.rects {
            self.add(*r);
        }
    }

    pub fn subtract(&mut self, cut: Rect) {
        if cut.is_empty() {
            return;
        }
        let mut out = Vec::with_capacity(self.rects.len());
        for r in &self.rects {
            r.subtract(&cut, &mut out);
        }
        self.rects = out;
    }

    /// Keeps only what lies inside `clip`.
    pub fn intersect(&mut self, clip: Rect) {
        self.rects = self.rects.iter().filter_map(|r| r.intersect(&clip)).collect();
    }

    pub fn translate(&mut self, dx: i32, dy: i32) {
        for r in &mut self.rects {
            *r = r.translate(dx, dy);
        }
    }

    pub fn bounding_box(&self) -> Rect {
        self.rects.iter().fold(Rect::default(), |acc, r| acc.union_box(r))
    }

    /// At most `max` rectangles covering at least the region: if it has
    /// more, its bounding box. (`FBIO_FLUSH` takes 16 per call.)
    pub fn coarsened(&self, max: usize) -> Vec<Rect> {
        if self.rects.len() <= max {
            self.rects.clone()
        } else {
            alloc::vec![self.bounding_box()]
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;

    const N: i32 = 24;

    /// The same region as a bitmap over an N x N grid: the oracle.
    struct Bits([[bool; N as usize]; N as usize]);

    impl Bits {
        fn set(&mut self, r: Rect, v: bool) {
            for y in 0..N {
                for x in 0..N {
                    if r.contains(x, y) {
                        self.0[y as usize][x as usize] = v;
                    }
                }
            }
        }
        fn clip(&mut self, r: Rect) {
            for y in 0..N {
                for x in 0..N {
                    if !r.contains(x, y) {
                        self.0[y as usize][x as usize] = false;
                    }
                }
            }
        }
    }

    fn check_same(g: &Region, b: &Bits) {
        // Disjoint: summed area equals the number of covered pixels.
        let mut count = 0;
        for y in 0..N {
            for x in 0..N {
                let inside = g.rects().iter().filter(|r| r.contains(x, y)).count();
                assert!(inside <= 1, "pixel ({x},{y}) in {inside} rects: {:?}", g.rects());
                assert_eq!(inside == 1, b.0[y as usize][x as usize], "pixel ({x},{y})");
                count += inside as i64;
            }
        }
        assert_eq!(g.area(), count);
        assert!(g.rects().iter().all(|r| !r.is_empty()));
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
        fn rect(&mut self) -> Rect {
            // Some rects stick out of the grid or are empty on purpose.
            let x = (self.next() % (N as u32 + 4)) as i32 - 2;
            let y = (self.next() % (N as u32 + 4)) as i32 - 2;
            let w = (self.next() % 12) as i32 - 1;
            let h = (self.next() % 12) as i32 - 1;
            Rect::new(x, y, w, h)
        }
    }

    #[test]
    fn random_ops_match_a_bitmap() {
        for seed in 0..300 {
            let mut rng = Lcg(seed);
            let mut g = Region::new();
            let mut b = Bits([[false; N as usize]; N as usize]);
            let everything = Rect::new(0, 0, N, N);
            for _ in 0..30 {
                let r = rng.rect();
                match rng.next() % 5 {
                    0 | 1 => {
                        g.add(r.intersect(&everything).unwrap_or_default());
                        b.set(r, true);
                    }
                    2 | 3 => {
                        g.subtract(r);
                        b.set(r, false);
                    }
                    _ => {
                        let c = rng.rect();
                        g.intersect(c);
                        b.clip(c);
                    }
                }
                check_same(&g, &b);
            }
        }
    }

    #[test]
    fn subtract_splits_into_four_around_a_hole() {
        let mut out = vec![];
        Rect::new(0, 0, 10, 10).subtract(&Rect::new(3, 4, 2, 2), &mut out);
        assert_eq!(out.len(), 4);
        assert_eq!(out.iter().map(Rect::area).sum::<i64>(), 96);
    }

    #[test]
    fn adding_what_is_covered_adds_nothing() {
        let mut g = Region::from_rect(Rect::new(0, 0, 10, 10));
        g.add(Rect::new(2, 2, 3, 3));
        assert_eq!(g.rects().len(), 1);
    }

    #[test]
    fn coarsened_falls_back_to_the_bounding_box() {
        let mut g = Region::new();
        for i in 0..20 {
            g.add(Rect::new(i * 3, 0, 1, 1));
        }
        assert_eq!(g.coarsened(16), vec![Rect::new(0, 0, 58, 1)]);
        assert_eq!(g.coarsened(20).len(), 20);
    }

    #[test]
    fn empty_and_edge_cases() {
        assert!(Rect::new(0, 0, 0, 5).is_empty());
        assert_eq!(Rect::new(0, 0, 5, 5).intersect(&Rect::new(5, 0, 5, 5)), None);
        let mut g = Region::new();
        g.add(Rect::new(0, 0, -3, 4));
        assert!(g.is_empty());
        assert_eq!(g.bounding_box(), Rect::default());
    }
}
