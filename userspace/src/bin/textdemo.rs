//! `textdemo` — phase 3 of `docs/gui/text-plan.md`: proportional Noto at
//! any size through `userspace::text` (parley + swash), in a window under
//! the compositor or on the console.
//!
//! Sizes from 10 to 72 px, accents and inverted marks, proportional
//! against monospaced, regular against bold, and a paragraph wrapped at a
//! width with a ruler under it as long as `measure` says it is.
//!
//! The layout is 960x640 logical pixels drawn at the screen's scale `k`
//! (`gfx::HIDPI`): coordinates and font sizes are multiplied by `k`, so
//! the glyphs are rasterised at the size they are shown at.
//!
//! For `scripts/gui-e2e.sh text` it prints, in window coordinates, the box
//! `measure` gave every checked piece of text (`textdemo: box <name> x y w
//! h`), which the script compares with the ink on a screendump; and the
//! timings the plan asks for: font loading, first frame, and all of
//! Latin-1 at 24 px with a cold cache and a warm one. Esc or Q quits.
//!
//! Disk-resident (`DISK_RUST_PROGRAMS`): parley + swash make it ~1.6 MB.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use core::fmt::Write;

use draw::Canvas;
use userspace::args::Args;
use userspace::gfx::{Gfx, EV_KEY, HIDPI};
use userspace::text::{Align, GlyphCache, Style, Text, MONO, SANS};
use userspace::{entry, println, syscall};

entry!(main);

const W: usize = 960;
const H: usize = 640;
const BG: u32 = 0x1E_2127;
const FG: u32 = 0xDC_DFE4;
const DIM: u32 = 0x8A_919E;
const ACCENT: u32 = 0x61_AFEF;
const WARM: u32 = 0xE5_C07B;
const RULER: u32 = 0x98_C379;
const KEY_ESC: u16 = 1;
const KEY_Q: u16 = 16;

const PHRASE: &str = "¿Él pidió ñandú y kiwi? ¡Sí!";
const PARAGRAPH: &str = "El veloz murciélago hindú comía feliz cardillo y kiwi. \
La cigüeña tocaba el saxofón detrás del palenque de paja. Añoraba \
Íñigo el pingüino aquel ñandú que, desde Ávila, cantaba «¡olé!».";
const PARA_WIDTH: f32 = 400.0;

/// Microseconds from `CLOCK_REALTIME` (boot RTC + monotonic uptime: it
/// does not jump while running).
fn now_us() -> i64 {
    let (s, ns) = syscall::clock_gettime();
    s * 1_000_000 + ns / 1000
}

/// Draws `s` and reports its measured box, for the e2e check.
fn item(t: &mut Text, cv: &mut Canvas, name: &str, s: &str, st: &Style, max: Option<f32>, x: i32, y: i32) -> (i32, i32) {
    let (w, h) = t.draw(cv, s, st, max, x, y);
    println!("textdemo: box {} {} {} {} {}", name, x, y, w, h);
    (w, h)
}

/// `Style::new` at `k` times `size`.
fn style(family: &'static str, size: f32, k: i32) -> Style<'static> {
    Style::new(family, size * k as f32)
}

fn picture(t: &mut Text, cv: &mut Canvas, k: i32) {
    let d = |v: i32| v * k;
    cv.fill(BG);

    let title = style(SANS, 26.0, k).bold().color(FG);
    item(t, cv, "title", "textdemo — Noto Sans con parley + swash", &title, None, d(24), d(14));

    // Left column: one line per size.
    let mut y = d(64);
    for size in [10, 12, 14, 16, 18, 20, 24, 28, 32] {
        let st = style(SANS, size as f32, k).color(if size % 4 == 0 { FG } else { DIM });
        let mut s = String::new();
        let _ = write!(s, "{size} px  {PHRASE}");
        let mut name = String::new();
        let _ = write!(name, "size{size}");
        let (_, h) = item(t, cv, &name, &s, &st, None, d(24), y);
        y += h;
    }

    // Right column: big sizes, centred on one axis.
    let mut y = d(64);
    for (size, s) in [(40, "Aa Ññ"), (56, "¿Qué?"), (72, "Ágil")] {
        let st = style(SANS, size as f32, k).bold().color(WARM).align(Align::Center);
        let (w, _) = t.measure(s, &st, None);
        let mut name = String::new();
        let _ = write!(name, "big{size}");
        let (_, h) = item(t, cv, &name, s, &st, None, d(800) - w / 2, y);
        y += h;
    }

    // Proportional against monospaced, regular against bold.
    let sample = "illimitado WWW 0123";
    let mut y = d(370);
    for (name, family, bold) in [("sans", SANS, false), ("sansbold", SANS, true), ("mono", MONO, false), ("monobold", MONO, true)] {
        let mut st = style(family, 20.0, k).color(ACCENT);
        if bold {
            st = st.bold();
        }
        let label = style(SANS, 14.0, k).color(DIM);
        t.draw(cv, name, &label, None, d(24), y + d(4));
        let (_, h) = item(t, cv, name, sample, &st, None, d(120), y);
        y += h + d(2);
    }

    // A paragraph wrapped at PARA_WIDTH, the width `measure` reports as a
    // ruler under it, and ticks above it where the limit is.
    let (px, py, limit) = (d(24), d(510), PARA_WIDTH * k as f32);
    let st = style(SANS, 15.0, k).color(FG);
    let (w, h) = item(t, cv, "para", PARAGRAPH, &st, Some(limit), px, py);
    cv.rect(px, py + h + d(3), w, d(2), RULER);
    cv.rect(px, py - d(6), k, d(4), RULER);
    cv.rect(px + limit as i32 - k, py - d(6), k, d(4), RULER);
    let note = style(SANS, 13.0, k).color(RULER);
    let mut s = String::new();
    let _ = write!(s, "measure: {w} × {h} px (límite {} px)", limit as i32);
    t.draw(cv, &s, &note, None, px, py + h + d(9));

    // The same paragraph centred in a narrower column. Not an `item`: a
    // centred line starts `(300 - its width) / 2` in, so the measured box
    // is not where its ink begins.
    let st = style(SANS, 15.0, k).color(DIM).align(Align::Center);
    t.draw(cv, PARAGRAPH, &st, Some(300.0 * k as f32), d(560), d(510));
}

/// All of Latin-1's printable characters at 24 px, laid out and drawn into
/// a scratch buffer with `cache`. Returns microseconds.
fn latin1(t: &mut Text, cache: &mut GlyphCache) -> i64 {
    let mut s = String::new();
    for c in (0x20u32..0x7F).chain(0xA1..0x100) {
        s.push(char::from_u32(c).unwrap());
    }
    let st = Style::new(SANS, 24.0).color(FG);
    let mut px = vec![BG; 900 * 300];
    let t0 = now_us();
    if let Some(l) = t.layout(&s, &st, Some(880.0)) {
        cache.draw(&mut Canvas::new(&mut px, 900, 300, 900), &l, 10, 10);
    }
    now_us() - t0
}

fn main(args: Args) -> i32 {
    let t0 = now_us();
    let mut t = Text::load();
    let t_load = now_us() - t0;
    if t.fonts() {
        println!("textdemo: fonts loaded in {} ms ({} missing)", t_load / 1000, t.missing);
    } else {
        println!("textdemo: no fonts in {} — bitmap fallback", userspace::text::FONT_DIR);
    }

    let Some(mut gfx) = Gfx::open(args.env(b"GUI_DISPLAY"), "textdemo", W, H, HIDPI) else {
        println!("textdemo: nothing to draw on (no /dev/fb, no compositor)");
        return 1;
    };

    let k = gfx.scale();
    let (fw, fh) = (W * k, H * k);
    println!("textdemo: scale {}", k);
    let mut frame = vec![BG; fw * fh];
    let t1 = now_us();
    picture(&mut t, &mut Canvas::new(&mut frame, fw, fh, fw), k as i32);
    let t_draw = now_us() - t1;
    gfx.present(&frame);
    let s = t.stats();
    println!(
        "textdemo: first frame {} ms ({} to present); cache {} hits {} misses, {} glyphs, {} KiB",
        t_draw / 1000,
        (now_us() - t1) / 1000,
        s.hits,
        s.misses,
        s.entries,
        s.bytes / 1024
    );

    if t.fonts() {
        let mut cache = GlyphCache::default();
        let cold = latin1(&mut t, &mut cache);
        let warm = latin1(&mut t, &mut cache);
        println!("textdemo: latin-1 at 24 px: cold {} ms, warm {} ms", cold / 1000, warm / 1000);
    }
    println!("textdemo: ready");

    loop {
        while let Some(ev) = gfx.next_event() {
            if ev.kind == EV_KEY && ev.value == 1 && (ev.code == KEY_ESC || ev.code == KEY_Q) {
                drop(gfx);
                println!("textdemo: bye");
                return 0;
            }
        }
        // Keeps the window's buffer current (a window only redraws what
        // it is sent) and paces us to the compositor.
        gfx.present(&frame);
        syscall::sleep_ms(50);
    }
}
