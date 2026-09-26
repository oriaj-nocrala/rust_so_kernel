//! Snake, drawn in 32-bit colour: a neon tube that glides between cells,
//! a glowing orb for food, particle bursts, and a pixel-font HUD.
//!
//! A 320x200 frame drawn with the `draw` crate and shown through
//! `userspace::gfx` — a window under the
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

use draw::color::{add, hsv, mix, scale};
use draw::font::digits;
use draw::{wave, Canvas, Font, FONT_3X5, FONT_5X7, FP};
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

// ── Text ─────────────────────────────────────────────────────────────────

/// Scale 1 is the 3x5 font, anything larger the 5x7 one.
fn font_for(sc: i32) -> Font {
    if sc == 1 { FONT_3X5 } else { FONT_5X7 }
}

fn text_width(s: &[u8], sc: i32) -> i32 {
    font_for(sc).width(s, sc)
}

/// Text with a drop shadow; `color` picks each glyph's colour by index.
fn text(cv: &mut Canvas, x: i32, y: i32, sc: i32, s: &[u8], color: impl Fn(usize) -> u32) {
    cv.text_shadowed(font_for(sc), x, y, sc, s, 0x05050A, color);
}

fn text_center(cv: &mut Canvas, y: i32, sc: i32, s: &[u8], c: u32) {
    let x = cv.center_x(font_for(sc), s, sc);
    text(cv, x, y, sc, s, |_| c);
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
    let mut frame = vec![0u32; W * H];
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
        let mut cv = Canvas::new(&mut frame, W, H, W);
        cv.blit(&bg, W, H, 0, 0);
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

        let shown: &[u32] = if g.shake > 0 {
            let (sx, sy) = (rng.range(-g.shake, g.shake + 1), rng.range(-g.shake, g.shake + 1));
            let mut out = Canvas::new(&mut shaken, W, H, W);
            out.fill(0);
            out.blit(&frame, W, H, sx, sy);
            &shaken
        } else {
            &frame
        };
        gfx.present(shown);

        // At most ~60 fps (in a window, present already waits for the
        // compositor).
        let spent = syscall::uptime_ms() - now;
        if spent < 16 {
            syscall::sleep_ms((16 - spent) as u64);
        }
    }
}
