//! Classic Snake — flagship demo for this OS.
//!
//! Renders on /dev/fb, a text console with ANSI escape support (not a raw
//! pixel framebuffer): "\x1b[2J" clears, "\x1b[{row};{col}H" positions the
//! cursor (1-based), "\x1b[3{n}m" sets a foreground color, "\x1b[0m" resets.
//!
//! Input comes from /dev/kbd, which is non-blocking: a read returns 0 bytes
//! immediately if no key is pending. Each tick we drain pending keys (taking
//! the last direction key seen), update the snake, redraw, then sleep.
//!
//! No heap anywhere here: the snake body is a fixed-capacity array of
//! (u8, u8) cells, and all number formatting goes through
//! `userspace::fmt::fprint` (backed by `core::fmt`, no allocation).

#![no_std]
#![no_main]

use userspace::{fmt::fprint, syscall};

// ── Playfield geometry ──────────────────────────────────────────────────
// Board is 20 columns x 15 rows, drawn a few terminal rows down so the
// score line has room above it.
const COLS: u8 = 20;
const ROWS: u8 = 15;
const TOP_OFFSET: u16 = 3; // terminal row where the board's row 0 is drawn
const LEFT_OFFSET: u16 = 2; // terminal col where the board's col 0 is drawn

const MAX_LEN: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    fn delta(self) -> (i8, i8) {
        match self {
            Dir::Up => (0, -1),
            Dir::Down => (0, 1),
            Dir::Left => (-1, 0),
            Dir::Right => (1, 0),
        }
    }

    fn is_reverse_of(self, other: Dir) -> bool {
        matches!(
            (self, other),
            (Dir::Up, Dir::Down)
                | (Dir::Down, Dir::Up)
                | (Dir::Left, Dir::Right)
                | (Dir::Right, Dir::Left)
        )
    }
}

/// Tiny xorshift32 PRNG, seeded from uptime so each run differs.
struct Rng(u32);

impl Rng {
    fn new(seed: u32) -> Self {
        Rng(if seed == 0 { 0xDEAD_BEEF } else { seed })
    }

    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// Random value in [0, bound).
    fn below(&mut self, bound: u32) -> u32 {
        self.next_u32() % bound
    }
}

struct Game {
    body: [(u8, u8); MAX_LEN],
    len: usize,
    dir: Dir,
    food: (u8, u8),
    score: u32,
    rng: Rng,
}

impl Game {
    fn new(rng_seed: u32) -> Self {
        let mut body = [(0u8, 0u8); MAX_LEN];
        let start = (COLS / 2, ROWS / 2);
        body[0] = start;
        body[1] = (start.0.wrapping_sub(1), start.1);
        body[2] = (start.0.wrapping_sub(2), start.1);

        let mut game = Game {
            body,
            len: 3,
            dir: Dir::Right,
            food: (0, 0),
            score: 0,
            rng: Rng::new(rng_seed),
        };
        game.food = game.spawn_food();
        game
    }

    fn head(&self) -> (u8, u8) {
        self.body[0]
    }

    fn occupies(&self, cell: (u8, u8)) -> bool {
        for i in 0..self.len {
            if self.body[i] == cell {
                return true;
            }
        }
        false
    }

    /// Picks a pseudo-random empty cell for food, retrying until it lands
    /// outside the snake body (bounded attempts to stay well clear of any
    /// infinite loop even in a near-full board).
    fn spawn_food(&mut self) -> (u8, u8) {
        for _ in 0..256 {
            let x = self.rng.below(COLS as u32) as u8;
            let y = self.rng.below(ROWS as u32) as u8;
            if !self.occupies((x, y)) {
                return (x, y);
            }
        }
        // Fallback: linear scan for the first free cell.
        for y in 0..ROWS {
            for x in 0..COLS {
                if !self.occupies((x, y)) {
                    return (x, y);
                }
            }
        }
        (0, 0)
    }

    /// Advances the snake one cell. Returns false on collision (game over).
    fn step(&mut self) -> bool {
        let (dx, dy) = self.dir.delta();
        let (hx, hy) = self.head();

        let nx = hx as i16 + dx as i16;
        let ny = hy as i16 + dy as i16;

        // Wall collision.
        if nx < 0 || ny < 0 || nx >= COLS as i16 || ny >= ROWS as i16 {
            return false;
        }
        let new_head = (nx as u8, ny as u8);

        let growing = new_head == self.food;

        // Self collision: the tail cell vacates this tick unless we're
        // growing, so it doesn't count as an obstacle in that case.
        let check_len = if growing { self.len } else { self.len.saturating_sub(1) };
        for i in 0..check_len {
            if self.body[i] == new_head {
                return false;
            }
        }

        // Shift body forward, growing if we ate food.
        let old_len = self.len;
        let new_len = if growing {
            (old_len + 1).min(MAX_LEN)
        } else {
            old_len
        };
        let mut i = new_len;
        while i > 1 {
            if i - 1 < old_len {
                self.body[i - 1] = self.body[i - 2];
            }
            i -= 1;
        }
        self.body[0] = new_head;
        self.len = new_len;

        if growing {
            self.score += 1;
            self.food = self.spawn_food();
        }

        true
    }
}

// ── Small no-alloc integer formatting helper for ANSI escapes ──────────────

/// Formats `n` (0..=999) as decimal ASCII into `buf`, returning the used
/// slice. Avoids needing `alloc`/`format!` for cursor-positioning escapes.
fn write_u32(buf: &mut [u8; 8], n: u32) -> &[u8] {
    if n == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }
    let mut tmp = [0u8; 8];
    let mut i = 0;
    let mut v = n;
    while v > 0 && i < tmp.len() {
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
        i += 1;
    }
    for j in 0..i {
        buf[j] = tmp[i - 1 - j];
    }
    &buf[..i]
}

fn move_cursor(fb_fd: i32, row: u16, col: u16) {
    let mut rbuf = [0u8; 8];
    let mut cbuf = [0u8; 8];
    let rs = write_u32(&mut rbuf, row as u32);
    let cs = write_u32(&mut cbuf, col as u32);
    syscall::write(fb_fd, b"\x1b[");
    syscall::write(fb_fd, rs);
    syscall::write(fb_fd, b";");
    syscall::write(fb_fd, cs);
    syscall::write(fb_fd, b"H");
}

fn draw(fb_fd: i32, game: &Game) {
    syscall::write(fb_fd, b"\x1b[2J");

    // Score line.
    move_cursor(fb_fd, 1, 1);
    fprint(fb_fd, format_args!("Snake -- Score: {}  (wasd to move, q to quit)", game.score));

    // Border + playfield.
    for y in 0..ROWS {
        move_cursor(fb_fd, TOP_OFFSET + y as u16, LEFT_OFFSET);
        syscall::write(fb_fd, b"\x1b[37m#"); // white border, left
        for x in 0..COLS {
            let cell = (x, y);
            if cell == game.head() {
                syscall::write(fb_fd, b"\x1b[32mO"); // green head
            } else if game.occupies(cell) {
                syscall::write(fb_fd, b"\x1b[32mo"); // green body
            } else if cell == game.food {
                syscall::write(fb_fd, b"\x1b[31m*"); // red food
            } else {
                syscall::write(fb_fd, b"\x1b[0m ");
            }
        }
        syscall::write(fb_fd, b"\x1b[37m#\x1b[0m"); // border, right
    }

    // Bottom border.
    move_cursor(fb_fd, TOP_OFFSET + ROWS as u16, LEFT_OFFSET);
    syscall::write(fb_fd, b"\x1b[37m");
    for _ in 0..(COLS as u16 + 2) {
        syscall::write(fb_fd, b"#");
    }
    syscall::write(fb_fd, b"\x1b[0m");
}

/// Drains all currently-pending keys from /dev/kbd, returning the last
/// direction/quit key seen this tick (later keys override earlier ones so a
/// burst of input doesn't lag behind by a tick).
fn poll_key(kbd_fd: i32) -> Option<u8> {
    let mut buf = [0u8; 1];
    let mut last = None;
    loop {
        let n = syscall::read(kbd_fd, &mut buf);
        if n <= 0 {
            break;
        }
        last = Some(buf[0]);
    }
    last
}

#[no_mangle]
extern "C" fn _start() -> ! {
    let fb_fd = syscall::with_cstr("/dev/fb", |p| syscall::open(p, 2)) as i32;
    let kbd_fd = syscall::with_cstr("/dev/kbd", |p| syscall::open(p, 0)) as i32;

    if fb_fd < 0 || kbd_fd < 0 {
        userspace::println!("snake: failed to open /dev/fb or /dev/kbd");
        syscall::exit(1);
    }

    let seed = syscall::uptime_ms() as u32;
    let mut game = Game::new(seed);

    draw(fb_fd, &game);

    loop {
        if let Some(key) = poll_key(kbd_fd) {
            let new_dir = match key {
                b'w' | b'W' => Some(Dir::Up),
                b's' | b'S' => Some(Dir::Down),
                b'a' | b'A' => Some(Dir::Left),
                b'd' | b'D' => Some(Dir::Right),
                b'q' | b'Q' => {
                    end_game(fb_fd, &game, false);
                }
                _ => None,
            };
            if let Some(nd) = new_dir {
                if !nd.is_reverse_of(game.dir) {
                    game.dir = nd;
                }
            }
        }

        if !game.step() {
            end_game(fb_fd, &game, true);
        }

        draw(fb_fd, &game);
        syscall::sleep_ms(150);
    }
}

/// Draws a final message, closes fds, and exits. Never returns.
fn end_game(fb_fd: i32, game: &Game, died: bool) -> ! {
    move_cursor(fb_fd, TOP_OFFSET + ROWS as u16 + 2, LEFT_OFFSET);
    syscall::write(fb_fd, b"\x1b[0m");
    if died {
        fprint(fb_fd, format_args!("Game Over! Score: {}\n", game.score));
    } else {
        fprint(fb_fd, format_args!("Quit. Score: {}\n", game.score));
    }
    syscall::close(fb_fd);
    userspace::println!("Game Over! Score: {}", game.score);
    syscall::exit(0);
}
