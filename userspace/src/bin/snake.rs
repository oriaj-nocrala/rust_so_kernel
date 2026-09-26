//! Snake, drawn in 32-bit colour: a neon tube that glides between cells,
//! a glowing orb for food, particle bursts, and a pixel-font HUD.
//!
//! A 320x200 frame through `userspace::gfx` — a window under the
//! compositor, the whole screen (`FBIO_BLIT`) on the console. The game
//! logic steps on a grid every `tick` ms (faster as the score grows); the
//! picture is drawn at up to 60 fps with every segment interpolated
//! between its last two cells, so the snake moves smoothly rather than
//! hopping. All arithmetic is integer: `x86_64-unknown-none` is soft-float.
//!
//! Arrows or WASD to steer, P to pause, Space/Enter to start, Esc/Q to
//! quit. The best score is kept in `/tmp/.snake_best` for the session.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use userspace::args::Args;
use userspace::gfx::{Gfx, EV_KEY};
use userspace::{entry, println, syscall};

entry!(main);

// ── Geometry ─────────────────────────────────────────────────────────────

const W: usize = 320;
const H: usize = 200;
const CELL: i32 = 10;
const GW: i32 = 32;
const GH: i32 = 18;
const HUD: i32 = 20;
/// Fixed point: 1/256 of a pixel.
const FP: i32 = 256;

const KEY_ESC: u16 = 1;
const KEY_Q: u16 = 16;
const KEY_W: u16 = 17;
const KEY_P: u16 = 25;
const KEY_ENTER: u16 = 28;
const KEY_A: u16 = 30;
const KEY_S: u16 = 31;
const KEY_D: u16 = 32;
const KEY_SPACE: u16 = 57;
const KEY_UP: u16 = 103;
const KEY_LEFT: u16 = 105;
const KEY_RIGHT: u16 = 106;
const KEY_DOWN: u16 = 108;

const FOOD: u32 = 0xFF3D7F;
const BEST_FILE: &str = "/tmp/.snake_best";

// ── Colour ───────────────────────────────────────────────────────────────

fn rgb(r: i32, g: i32, b: i32) -> u32 {
    (r.clamp(0, 255) as u32) << 16 | (g.clamp(0, 255) as u32) << 8 | b.clamp(0, 255) as u32
}

fn parts(c: u32) -> (i32, i32, i32) {
    ((c >> 16 & 0xFF) as i32, (c >> 8 & 0xFF) as i32, (c & 0xFF) as i32)
}

/// `c` scaled by `k`/256.
fn scale(c: u32, k: i32) -> u32 {
    let (r, g, b) = parts(c);
    rgb(r * k >> 8, g * k >> 8, b * k >> 8)
}

/// `a` → `b` by `t`/256.
fn mix(a: u32, b: u32, t: i32) -> u32 {
    let (ar, ag, ab) = parts(a);
    let (br, bg, bb) = parts(b);
    rgb(ar + ((br - ar) * t >> 8), ag + ((bg - ag) * t >> 8), ab + ((bb - ab) * t >> 8))
}

fn add(a: u32, b: u32) -> u32 {
    let (ar, ag, ab) = parts(a);
    let (br, bg, bb) = parts(b);
    rgb(ar + br, ag + bg, ab + bb)
}

/// Hue 0..1536 (six 256-wide sextants), saturation and value 0..255.
fn hsv(h: i32, s: i32, v: i32) -> u32 {
    let h = h.rem_euclid(1536);
    let f = h & 255;
    let p = v * (255 - s) / 255;
    let q = v * (255 - s * f / 255) / 255;
    let t = v * (255 - s * (255 - f) / 255) / 255;
    match h >> 8 {
        0 => rgb(v, t, p),
        1 => rgb(q, v, p),
        2 => rgb(p, v, t),
        3 => rgb(p, q, v),
        4 => rgb(t, p, v),
        _ => rgb(v, p, q),
    }
}

/// A sine-shaped wave in -256..=256 with period `period` ms (parabolic
/// approximation — no libm here).
fn wave(t: i64, period: i64) -> i32 {
    let p = (t.rem_euclid(period) * 1024 / period) as i32;
    let (sign, q) = if p < 512 { (1, p) } else { (-1, p - 512) };
    sign * q * (512 - q) / 256
}

// ── Canvas ───────────────────────────────────────────────────────────────

struct Canvas {
    px: Vec<u32>,
}

impl Canvas {
    fn put(&mut self, x: i32, y: i32, c: u32) {
        if x >= 0 && y >= 0 && (x as usize) < W && (y as usize) < H {
            self.px[y as usize * W + x as usize] = c;
        }
    }

    fn get(&self, x: i32, y: i32) -> u32 {
        self.px[y as usize * W + x as usize]
    }

    fn add_px(&mut self, x: i32, y: i32, c: u32) {
        if x >= 0 && y >= 0 && (x as usize) < W && (y as usize) < H {
            let i = y as usize * W + x as usize;
            self.px[i] = add(self.px[i], c);
        }
    }

    fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, c: u32) {
        for yy in y.max(0)..(y + h).min(H as i32) {
            for xx in x.max(0)..(x + w).min(W as i32) {
                self.px[yy as usize * W + xx as usize] = c;
            }
        }
    }

    /// An antialiased disc at a fixed-point centre. With `shade`, darker
    /// towards the rim and lit from the top left, so a row of them reads
    /// as a tube.
    fn disc(&mut self, cx: i32, cy: i32, r: i32, c: u32, shade: bool) {
        let x0 = ((cx - r) / FP - 1).max(0);
        let x1 = ((cx + r) / FP + 1).min(W as i32 - 1);
        let y0 = ((cy - r) / FP - 1).max(0);
        let y1 = ((cy + r) / FP + 1).min(H as i32 - 1);
        let r2 = r as i64 * r as i64;
        for y in y0..=y1 {
            let dy = (y * FP + FP / 2 - cy) as i64;
            for x in x0..=x1 {
                let dx = (x * FP + FP / 2 - cx) as i64;
                let d2 = dx * dx + dy * dy;
                if d2 >= r2 {
                    continue;
                }
                // r - d ≈ (r² - d²) / 2r: coverage of the rim pixel.
                let cov = ((r2 - d2) / (2 * r as i64)).min(FP as i64) as i32;
                let mut col = c;
                if shade {
                    let rim = (d2 * 110 / r2) as i32;
                    let light = ((-dx - dy) * 60 / (2 * r as i64)) as i32;
                    col = scale(c, 256 - rim + light.max(0));
                }
                let i = y as usize * W + x as usize;
                self.px[i] = if cov >= FP { col } else { mix(self.px[i], col, cov) };
            }
        }
    }

    /// Additive light with a quadratic falloff out to `r`.
    fn glow(&mut self, cx: i32, cy: i32, r: i32, c: u32, strength: i32) {
        let x0 = ((cx - r) / FP).max(0);
        let x1 = ((cx + r) / FP).min(W as i32 - 1);
        let y0 = ((cy - r) / FP).max(0);
        let y1 = ((cy + r) / FP).min(H as i32 - 1);
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
                let i = y as usize * W + x as usize;
                self.px[i] = add(self.px[i], scale(c, k));
            }
        }
    }

    fn dim(&mut self, k: i32) {
        for p in self.px.iter_mut() {
            *p = scale(*p, k);
        }
    }
}

// ── Pixel font (3x5) ─────────────────────────────────────────────────────

/// The 3x5 font, for small print.
fn glyph(ch: u8) -> [u8; 5] {
    match ch {
        b'0' => [7, 5, 5, 5, 7],
        b'1' => [2, 6, 2, 2, 7],
        b'2' => [7, 1, 7, 4, 7],
        b'3' => [7, 1, 7, 1, 7],
        b'4' => [5, 5, 7, 1, 1],
        b'5' => [7, 4, 7, 1, 7],
        b'6' => [7, 4, 7, 5, 7],
        b'7' => [7, 1, 2, 2, 2],
        b'8' => [7, 5, 7, 5, 7],
        b'9' => [7, 5, 7, 1, 7],
        b'A' => [2, 5, 7, 5, 5],
        b'B' => [6, 5, 6, 5, 6],
        b'C' => [3, 4, 4, 4, 3],
        b'D' => [6, 5, 5, 5, 6],
        b'E' => [7, 4, 6, 4, 7],
        b'F' => [7, 4, 6, 4, 4],
        b'G' => [3, 4, 5, 5, 3],
        b'H' => [5, 5, 7, 5, 5],
        b'I' => [7, 2, 2, 2, 7],
        b'J' => [1, 1, 1, 5, 2],
        b'K' => [5, 5, 6, 5, 5],
        b'L' => [4, 4, 4, 4, 7],
        b'M' => [5, 7, 7, 5, 5],
        b'N' => [6, 5, 5, 5, 5],
        b'O' => [2, 5, 5, 5, 2],
        b'P' => [6, 5, 6, 4, 4],
        b'Q' => [2, 5, 5, 6, 3],
        b'R' => [6, 5, 6, 5, 5],
        b'S' => [3, 4, 2, 1, 6],
        b'T' => [7, 2, 2, 2, 2],
        b'U' => [5, 5, 5, 5, 7],
        b'V' => [5, 5, 5, 5, 2],
        b'W' => [5, 5, 7, 7, 5],
        b'X' => [5, 5, 2, 5, 5],
        b'Y' => [5, 5, 2, 2, 2],
        b'Z' => [7, 1, 2, 4, 7],
        b':' => [0, 2, 0, 2, 0],
        b'!' => [2, 2, 2, 0, 2],
        b'-' => [0, 0, 7, 0, 0],
        b'/' => [1, 1, 2, 4, 4],
        _ => [0; 5],
    }
}

/// The 5x7 font, for everything drawn at scale 2 and up.
fn glyph7(ch: u8) -> [u8; 7] {
    match ch {
        b'0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        b'1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        b'2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        b'3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        b'4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        b'5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        b'6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        b'7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        b'8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        b'9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        b'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        b'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        b'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        b'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
        b'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        b'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        b'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        b'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        b'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        b'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        b'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        b'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        b'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        b'N' => [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11],
        b'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        b'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        b'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        b'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        b'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        b'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        b'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        b'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        b'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A],
        b'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        b'Y' => [0x11, 0x11, 0x11, 0x0A, 0x04, 0x04, 0x04],
        b'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        b'!' => [0x04, 0x04, 0x04, 0x04, 0x04, 0x00, 0x04],
        b':' => [0x00, 0x0C, 0x0C, 0x00, 0x0C, 0x0C, 0x00],
        b'-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        b'/' => [0x01, 0x01, 0x02, 0x04, 0x08, 0x10, 0x10],
        _ => [0; 7],
    }
}

/// Scale 1 is the 3x5 font; anything larger, the 5x7 one.
fn text_width(s: &[u8], sc: i32) -> i32 {
    if sc == 1 { s.len() as i32 * 4 - 1 } else { (s.len() as i32 * 6 - 1) * sc }
}

/// Text with a drop shadow; `color` picks each glyph's colour by index.
fn text(cv: &mut Canvas, x: i32, y: i32, sc: i32, s: &[u8], color: impl Fn(usize) -> u32) {
    let (cols, adv) = if sc == 1 { (3, 4) } else { (5, 6) };
    for (i, &ch) in s.iter().enumerate() {
        let rows: [u8; 7] = if sc == 1 {
            let g = glyph(ch);
            [g[0], g[1], g[2], g[3], g[4], 0, 0]
        } else {
            glyph7(ch)
        };
        let gx = x + i as i32 * adv * sc;
        let c = color(i);
        for (row, bits) in rows.iter().enumerate() {
            for col in 0..cols {
                if bits & (1 << (cols - 1 - col)) != 0 {
                    let px = gx + col * sc;
                    let py = y + row as i32 * sc;
                    cv.rect(px + sc.max(2) / 2, py + sc.max(2) / 2, sc, sc, 0x05050A);
                    cv.rect(px, py, sc, sc, c);
                }
            }
        }
    }
}

fn text_center(cv: &mut Canvas, y: i32, sc: i32, s: &[u8], c: u32) {
    text(cv, (W as i32 - text_width(s, sc)) / 2, y, sc, s, |_| c);
}

/// Decimal digits of `n` into `buf`.
fn digits(n: u32, buf: &mut [u8; 10]) -> &[u8] {
    let mut i = buf.len();
    let mut v = n;
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    &buf[i..]
}

// ── Randomness and particles ─────────────────────────────────────────────

struct Rng(u32);

impl Rng {
    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// In `lo..hi`.
    fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next() % (hi - lo) as u32) as i32
    }
}

#[derive(Clone, Copy)]
struct Particle {
    x: i32,
    y: i32,
    vx: i32,
    vy: i32,
    life: i32,
    max: i32,
    c: u32,
}

struct Particles(Vec<Particle>);

impl Particles {
    /// `n` sparks from a fixed-point point, at up to `speed` fp/frame.
    fn burst(&mut self, rng: &mut Rng, x: i32, y: i32, n: usize, speed: i32, c: u32) {
        for _ in 0..n {
            let (vx, vy) = loop {
                let vx = rng.range(-speed, speed + 1);
                let vy = rng.range(-speed, speed + 1);
                if vx as i64 * vx as i64 + vy as i64 * vy as i64 <= speed as i64 * speed as i64 {
                    break (vx, vy);
                }
            };
            let life = rng.range(350, 900);
            let c = if rng.next() % 4 == 0 { 0xFFFFFF } else { c };
            if self.0.len() < 600 {
                self.0.push(Particle { x, y, vx, vy, life, max: life, c });
            }
        }
    }

    /// Advances by `dt` ms (velocities are per 16 ms frame).
    fn update(&mut self, dt: i32) {
        for p in self.0.iter_mut() {
            p.x += p.vx * dt / 16;
            p.y += p.vy * dt / 16;
            p.vy += 6 * dt / 16; // a little gravity
            p.vx -= p.vx * dt / 400; // and drag
            p.life -= dt;
        }
        self.0.retain(|p| p.life > 0);
    }

    fn draw(&self, cv: &mut Canvas) {
        for p in &self.0 {
            let c = scale(p.c, p.life * 256 / p.max);
            let (x, y) = (p.x / FP, p.y / FP);
            cv.add_px(x, y, c);
            cv.add_px(x + 1, y, scale(c, 140));
            cv.add_px(x, y + 1, scale(c, 140));
        }
    }
}

// ── The game ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    fn delta(self) -> (i32, i32) {
        match self {
            Dir::Up => (0, -1),
            Dir::Down => (0, 1),
            Dir::Left => (-1, 0),
            Dir::Right => (1, 0),
        }
    }

    fn opposite(self) -> Dir {
        match self {
            Dir::Up => Dir::Down,
            Dir::Down => Dir::Up,
            Dir::Left => Dir::Right,
            Dir::Right => Dir::Left,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Title,
    Play,
    Paused,
    /// Crashed at this time: the snake flashes, then bursts.
    Dying(i64),
    Over,
}

struct Game {
    /// Cells, head first; `prev[i]` is where segment `i` was before the
    /// last step, which is what the drawing interpolates from.
    body: Vec<(i32, i32)>,
    prev: Vec<(i32, i32)>,
    dir: Dir,
    /// Turns typed faster than the snake steps, applied one per step.
    turns: [Dir; 3],
    nturns: usize,
    food: (i32, i32),
    food_born: i64,
    score: u32,
    best: u32,
    new_best: bool,
    acc: i32,
    state: State,
    /// Screen shake strength in pixels, and when the last score happened.
    shake: i32,
    scored_at: i64,
}

impl Game {
    fn new(best: u32) -> Game {
        Game {
            body: Vec::new(),
            prev: Vec::new(),
            dir: Dir::Right,
            turns: [Dir::Right; 3],
            nturns: 0,
            food: (0, 0),
            food_born: 0,
            score: 0,
            best,
            new_best: false,
            acc: 0,
            state: State::Title,
            shake: 0,
            scored_at: -10_000,
        }
    }

    fn start(&mut self, rng: &mut Rng, now: i64) {
        self.body = vec![(8, GH / 2), (7, GH / 2), (6, GH / 2), (5, GH / 2)];
        self.prev = self.body.clone();
        self.dir = Dir::Right;
        self.nturns = 0;
        self.score = 0;
        self.new_best = false;
        self.acc = 0;
        self.spawn_food(rng, now);
        self.state = State::Play;
    }

    fn tick_ms(&self) -> i32 {
        (130 - 3 * self.score as i32).max(55)
    }

    fn spawn_food(&mut self, rng: &mut Rng, now: i64) {
        loop {
            let c = (rng.range(0, GW), rng.range(0, GH));
            if !self.body.contains(&c) {
                self.food = c;
                self.food_born = now;
                return;
            }
        }
    }

    fn turn(&mut self, d: Dir) {
        let last = if self.nturns > 0 { self.turns[self.nturns - 1] } else { self.dir };
        if d != last && d != last.opposite() && self.nturns < self.turns.len() {
            self.turns[self.nturns] = d;
            self.nturns += 1;
        }
    }

    /// One grid step. False on a crash (the snake stays where it was).
    fn step(&mut self, rng: &mut Rng, parts: &mut Particles, now: i64) -> bool {
        if self.nturns > 0 {
            self.dir = self.turns[0];
            self.turns.copy_within(1.., 0);
            self.nturns -= 1;
        }
        let (dx, dy) = self.dir.delta();
        let head = (self.body[0].0 + dx, self.body[0].1 + dy);
        let eating = head == self.food;
        let tail_moves = !eating;
        let blocked = self.body[..self.body.len() - tail_moves as usize].contains(&head);
        if head.0 < 0 || head.1 < 0 || head.0 >= GW || head.1 >= GH || blocked {
            self.prev = self.body.clone();
            return false;
        }
        self.prev = self.body.clone();
        let tail = *self.body.last().unwrap();
        self.body.pop();
        self.body.insert(0, head);
        if eating {
            self.body.push(tail);
            self.prev.push(tail);
            self.score += 1;
            self.scored_at = now;
            let (fx, fy) = center(self.food);
            parts.burst(rng, fx, fy, 36, 3 * FP / 2, FOOD);
            self.spawn_food(rng, now);
        }
        true
    }
}

/// The fixed-point pixel centre of a cell.
fn center((x, y): (i32, i32)) -> (i32, i32) {
    ((x * CELL + CELL / 2) * FP, (HUD + y * CELL + CELL / 2) * FP)
}

fn lerp(a: i32, b: i32, t: i32) -> i32 {
    a + ((b - a) as i64 * t as i64 / 256) as i32
}

// ── Drawing ──────────────────────────────────────────────────────────────

/// The static backdrop: a dark violet gradient with a faint checkerboard
/// and a vignette, under a HUD strip.
fn backdrop() -> Vec<u32> {
    let mut bg = vec![0u32; W * H];
    for y in 0..H as i32 {
        for x in 0..W as i32 {
            let c = if y < HUD {
                0x070812
            } else {
                let t = (y - HUD) * 256 / (H as i32 - HUD);
                let mut c = mix(0x0E1124, 0x1A1030, t);
                if ((x / CELL) + ((y - HUD) / CELL)) % 2 == 1 {
                    c = add(c, 0x040406);
                }
                let dx = (x - W as i32 / 2) as i64;
                let dy = (y - (HUD + GH * CELL / 2)) as i64;
                let d2 = dx * dx + dy * dy * 3;
                c = scale(c, 256 - (d2 * 110 / (160 * 160 + 90 * 90 * 3)) as i32);
                c
            };
            bg[y as usize * W + x as usize] = c & 0xFFFFFF;
        }
    }
    bg
}

fn draw_hud(cv: &mut Canvas, g: &Game, now: i64) {
    // A rainbow rule under the HUD, flowing.
    for x in 0..W as i32 {
        cv.put(x, HUD - 1, hsv(x * 5 + (now / 3) as i32, 200, 230));
    }
    let mut b = [0u8; 10];
    text(cv, 6, 3, 2, b"SCORE", |_| 0x8088B0);
    let pop = (now - g.scored_at) as i32;
    let score_c = if pop < 300 { mix(0xFFFFFF, FOOD, pop * 256 / 300) } else { 0xFFFFFF };
    let s = digits(g.score, &mut b);
    text(cv, 6 + text_width(b"SCORE ", 2), 3, 2, s, |_| score_c);

    let mut b2 = [0u8; 10];
    let best = digits(g.best, &mut b2);
    let bx = W as i32 - 6 - text_width(best, 2);
    text(cv, bx, 3, 2, best, |_| 0xFFD35C);
    text(cv, bx - text_width(b"BEST ", 2), 3, 2, b"BEST", |_| 0x8088B0);
}

fn draw_food(cv: &mut Canvas, g: &Game, now: i64) {
    let (fx, fy) = center(g.food);
    let born = ((now - g.food_born) as i32 * 256 / 220).min(256);
    let pulse = wave(now, 900);
    let r = (4 * FP + pulse * FP / 256 * 3 / 4) * born / 256;
    cv.glow(fx, fy, 16 * FP * born / 256, FOOD, 150 + pulse / 4);
    cv.disc(fx, fy, r, FOOD, true);
    cv.disc(fx - r / 3, fy - r / 3, r / 3, 0xFFD0E0, false);
}

fn draw_snake(cv: &mut Canvas, g: &Game, t: i32, now: i64, flash: Option<u32>) {
    let n = g.body.len();
    let pos = |i: usize| {
        let (ax, ay) = center(g.prev[i]);
        let (bx, by) = center(g.body[i]);
        (lerp(ax, bx, t), lerp(ay, by, t))
    };
    let radius = |i: usize| 5 * FP - (2 * FP * i as i32) / (n as i32).max(8);
    let color = |i: usize| match flash {
        Some(c) => c,
        None => hsv((now / 6) as i32 - i as i32 * 60 + 640, 190, 255),
    };

    // Tail first so the head ends up on top; each link is a row of discs
    // from this segment to the next one towards the head.
    for i in (0..n).rev() {
        let (x, y) = pos(i);
        let (r, c) = (radius(i), color(i));
        if i > 0 {
            let (nx, ny) = pos(i - 1);
            let steps = ((nx - x).abs().max((ny - y).abs()) / (FP * 3 / 2)).max(1);
            for s in 0..steps {
                let k = s * 256 / steps;
                cv.disc(lerp(x, nx, k), lerp(y, ny, k), lerp(r, radius(i - 1), k), mix(c, color(i - 1), k), true);
            }
        } else {
            cv.disc(x, y, r, c, true);
        }
    }

    // Head: a halo and two eyes looking where it goes.
    let (hx, hy) = pos(0);
    cv.glow(hx, hy, 14 * FP, color(0), 90);
    cv.disc(hx, hy, radius(0), color(0), true);
    let (dx, dy) = g.dir.delta();
    let (px, py) = (-dy, dx);
    for side in [-1, 1] {
        let ex = hx + dx * 2 * FP + px * side * 5 * FP / 2;
        let ey = hy + dy * 2 * FP + py * side * 5 * FP / 2;
        cv.disc(ex, ey, 7 * FP / 4, 0xFFFFFF, false);
        cv.disc(ex + dx * FP * 2 / 3, ey + dy * FP * 2 / 3, FP, 0x10101A, false);
    }
}

fn draw_title(cv: &mut Canvas, g: &Game, now: i64) {
    // A rainbow worm swimming across behind the title.
    for i in (0..44).rev() {
        let x = ((now / 12 + i as i64 * 9) % (W as i64 + 120)) as i32 - 60;
        let y = 150 + wave(now - i as i64 * 40, 1600) * 14 / 256;
        let c = hsv((now / 6) as i32 - i * 36, 190, 255);
        cv.disc(x * FP, y * FP, (5 * FP) - i * FP / 20, c, true);
    }
    let title = b"SNAKE";
    let sc = 8;
    let x0 = (W as i32 - text_width(title, sc)) / 2;
    for (i, &ch) in title.iter().enumerate() {
        let bob = wave(now - i as i64 * 120, 1400) * 4 / 256;
        let c = hsv((now / 5) as i32 + i as i32 * 200, 170, 255);
        text(cv, x0 + i as i32 * 6 * sc, 30 + bob, sc, &[ch], |_| c);
    }
    if (now / 500) % 2 == 0 {
        text_center(cv, 104, 2, b"PRESS SPACE TO PLAY", 0xFFFFFF);
    }
    text_center(cv, 187, 1, b"ARROWS/WASD MOVE - P PAUSE - ESC QUIT", 0x8088B0);
    if g.best > 0 {
        let mut b = [0u8; 10];
        let s = digits(g.best, &mut b);
        let w = text_width(b"BEST ", 2) + text_width(s, 2);
        let x = (W as i32 - w) / 2;
        text(cv, x, 124, 2, b"BEST", |_| 0x8088B0);
        text(cv, x + text_width(b"BEST ", 2), 124, 2, s, |_| 0xFFD35C);
    }
}

// ── Best score ───────────────────────────────────────────────────────────

fn load_best() -> u32 {
    let fd = syscall::with_cstr(BEST_FILE, |p| syscall::open(p, syscall::O_RDONLY)) as i32;
    if fd < 0 {
        return 0;
    }
    let mut buf = [0u8; 12];
    let n = syscall::read(fd, &mut buf).max(0) as usize;
    syscall::close(fd);
    buf[..n].iter().take_while(|b| b.is_ascii_digit()).fold(0u32, |a, &d| a.saturating_mul(10).saturating_add((d - b'0') as u32))
}

fn save_best(best: u32) {
    let flags = syscall::O_WRONLY | syscall::O_CREAT | syscall::O_TRUNC;
    let fd = syscall::with_cstr(BEST_FILE, |p| syscall::open(p, flags)) as i32;
    if fd >= 0 {
        let mut b = [0u8; 10];
        syscall::write(fd, digits(best, &mut b));
        syscall::close(fd);
    }
}

// ── Main loop ────────────────────────────────────────────────────────────

fn main(args: Args) -> i32 {
    let Some(mut gfx) = Gfx::open(args.env(b"GUI_DISPLAY"), "snake", W, H, 0) else {
        println!("snake: nothing to draw on (no /dev/fb, no compositor)");
        return 1;
    };

    let (_, nsec) = syscall::clock_gettime();
    let mut rng = Rng((syscall::uptime_ms() as u32 ^ nsec as u32) | 1);
    let bg = backdrop();
    let mut cv = Canvas { px: vec![0; W * H] };
    let mut shaken = vec![0u32; W * H];
    let mut parts = Particles(Vec::new());
    let mut g = Game::new(load_best());
    let mut last = syscall::uptime_ms();

    loop {
        let now = syscall::uptime_ms();
        let dt = (now - last).clamp(0, 50) as i32;
        last = now;

        // Input.
        while let Some(ev) = gfx.next_event() {
            if ev.kind != EV_KEY || ev.value == 0 {
                continue;
            }
            let fresh = ev.value == 1; // not an autorepeat
            let dir = match ev.code {
                KEY_UP | KEY_W => Some(Dir::Up),
                KEY_DOWN | KEY_S => Some(Dir::Down),
                KEY_LEFT | KEY_A => Some(Dir::Left),
                KEY_RIGHT | KEY_D => Some(Dir::Right),
                _ => None,
            };
            match (g.state, ev.code) {
                (_, KEY_ESC | KEY_Q) if fresh => {
                    drop(gfx);
                    println!("snake: score {}, best {}", g.score, g.best);
                    return 0;
                }
                (State::Title | State::Over, KEY_SPACE | KEY_ENTER) if fresh => g.start(&mut rng, now),
                (State::Play, KEY_P) if fresh => g.state = State::Paused,
                (State::Paused, KEY_P | KEY_SPACE | KEY_ENTER) if fresh => g.state = State::Play,
                (State::Play, _) => {
                    if let Some(d) = dir {
                        g.turn(d);
                    }
                }
                _ => {}
            }
        }

        // Simulation.
        let mut t = 256;
        match g.state {
            State::Play => {
                g.acc += dt;
                while g.acc >= g.tick_ms() {
                    g.acc -= g.tick_ms();
                    if !g.step(&mut rng, &mut parts, now) {
                        g.state = State::Dying(now);
                        g.shake = 6;
                        if g.score > g.best {
                            g.best = g.score;
                            g.new_best = true;
                            save_best(g.best);
                        }
                        break;
                    }
                }
                if g.state == State::Play {
                    t = g.acc * 256 / g.tick_ms();
                }
            }
            State::Dying(at) if now - at > 650 => {
                for (i, &c) in g.body.iter().enumerate() {
                    let (x, y) = center(c);
                    let col = hsv((now / 6) as i32 - i as i32 * 60 + 640, 190, 255);
                    parts.burst(&mut rng, x, y, 10, 2 * FP, col);
                }
                g.shake = 8;
                g.state = State::Over;
            }
            _ => {}
        }
        parts.update(dt);
        if g.shake > 0 && rng.next() % 3 == 0 {
            g.shake -= 1;
        }

        // Picture.
        cv.px.copy_from_slice(&bg);
        match g.state {
            State::Title => draw_title(&mut cv, &g, now),
            State::Play | State::Paused => {
                draw_food(&mut cv, &g, now);
                draw_snake(&mut cv, &g, t, now, None);
            }
            State::Dying(at) => {
                draw_food(&mut cv, &g, now);
                let flash = if ((now - at) / 90) % 2 == 0 { 0xFFFFFF } else { 0xFF2A4A };
                draw_snake(&mut cv, &g, 256, now, Some(flash));
            }
            State::Over => {}
        }
        if g.state != State::Title {
            draw_hud(&mut cv, &g, now);
        }
        match g.state {
            State::Paused => {
                cv.dim(110);
                text_center(&mut cv, 84, 4, b"PAUSED", 0xFFFFFF);
                text_center(&mut cv, 122, 1, b"P TO RESUME", 0x8088B0);
            }
            State::Over => {
                cv.dim(120);
                parts.draw(&mut cv);
                text(&mut cv, (W as i32 - text_width(b"GAME OVER", 5)) / 2, 44, 5, b"GAME OVER", |i| {
                    hsv(1480 - i as i32 * 30, 200, 255)
                });
                let mut b = [0u8; 10];
                let s = digits(g.score, &mut b);
                let w = text_width(b"SCORE ", 3) + text_width(s, 3);
                let x = (W as i32 - w) / 2;
                text(&mut cv, x, 92, 3, b"SCORE", |_| 0x8088B0);
                text(&mut cv, x + text_width(b"SCORE ", 3), 92, 3, s, |_| 0xFFFFFF);
                if g.new_best {
                    let c = hsv((now / 3) as i32, 160, 255);
                    text_center(&mut cv, 122, 2, b"NEW BEST!", c);
                }
                if (now / 500) % 2 == 0 {
                    text_center(&mut cv, 148, 2, b"SPACE TO PLAY AGAIN", 0xFFFFFF);
                }
            }
            _ => parts.draw(&mut cv),
        }

        let frame: &[u32] = if g.shake > 0 {
            let (sx, sy) = (rng.range(-g.shake, g.shake + 1), rng.range(-g.shake, g.shake + 1));
            for y in 0..H as i32 {
                for x in 0..W as i32 {
                    let (ox, oy) = (x - sx, y - sy);
                    shaken[y as usize * W + x as usize] =
                        if ox >= 0 && oy >= 0 && ox < W as i32 && oy < H as i32 { cv.get(ox, oy) } else { 0 };
                }
            }
            &shaken
        } else {
            &cv.px
        };
        gfx.present(frame);

        // At most ~60 fps (in a window, present already waits for the
        // compositor).
        let spent = syscall::uptime_ms() - now;
        if spent < 16 {
            syscall::sleep_ms((16 - spent) as u64);
        }
    }
}
