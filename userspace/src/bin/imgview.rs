//! `imgview [file.png ...]` — shows PNG images (default: the sample icon
//! in `/mnt/usr/share/icons`) over a checkerboard and over a solid colour,
//! through `userspace::img` (decode) and `Canvas::blit_over` (AVX2
//! premultiplied "over" when the CPU has it).
//!
//! Drawn 1:1 in screen pixels: on a `HIDPI` screen the images come out
//! smaller, not scaled.
//!
//! Prints, per image, its size and decode time, then which blend path is
//! live and what one `blit_over` of each image costs (averaged over many),
//! so the AVX2 path can be checked on the machine itself. Esc or Q quits.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use draw::Canvas;
use userspace::args::Args;
use userspace::gfx::{Gfx, EV_KEY, HIDPI};
use userspace::img::{self, Image, ICON_DIR};
use userspace::{entry, println, syscall};

entry!(main);

const W: usize = 640;
const H: usize = 400;
const LIGHT: u32 = 0xC8_C8C8;
const DARK: u32 = 0x78_7878;
const SOLID: u32 = 0x1E_2127;
const KEY_ESC: u16 = 1;
const KEY_Q: u16 = 16;
const BLITS: u32 = 2000;

fn now_us() -> i64 {
    let (s, ns) = syscall::clock_gettime();
    s * 1_000_000 + ns / 1000
}

/// Squares of `side` pixels, light and dark, so transparency shows.
fn checkerboard(cv: &mut Canvas, side: i32) {
    for y in (0..cv.height()).step_by(side as usize) {
        for x in (0..cv.width()).step_by(side as usize) {
            let c = if (x / side + y / side) % 2 == 0 { LIGHT } else { DARK };
            cv.rect(x, y, side, side, c);
        }
    }
}

fn main(args: Args) -> i32 {
    let mut paths: Vec<&str> = (1..args.len())
        .filter_map(|i| args.get(i))
        .filter_map(|a| core::str::from_utf8(a).ok())
        .collect();
    let default = alloc::format!("{}/sample.png", ICON_DIR);
    if paths.is_empty() {
        paths.push(&default);
    }

    let mut images: Vec<Image> = Vec::new();
    for path in &paths {
        let t0 = now_us();
        match img::load(path) {
            Ok(im) => {
                println!("imgview: {} {}x{} decoded in {} us", path, im.w, im.h, now_us() - t0);
                images.push(im);
            }
            Err(e) => println!("imgview: {}: {}", path, e),
        }
    }
    if images.is_empty() {
        return 1;
    }

    let Some(mut gfx) = Gfx::open(args.env(b"GUI_DISPLAY"), "imgview", W, H, HIDPI) else {
        println!("imgview: nothing to draw on (no /dev/fb, no compositor)");
        return 1;
    };
    let k = gfx.scale();
    let (fw, fh) = (W * k, H * k);
    let mut frame = vec![SOLID; fw * fh];
    let mut cv = Canvas::new(&mut frame, fw, fh, fw);

    // Top half checkerboard, bottom half solid; every image in a row on
    // each, left to right.
    checkerboard(&mut cv, 8 * k as i32);
    cv.rect(0, fh as i32 / 2, fw as i32, fh as i32 / 2, SOLID);
    let mut x = 16 * k as i32;
    for im in &images {
        cv.blit_over(&im.px, im.w, im.h, x, 16 * k as i32);
        cv.blit_over(&im.px, im.w, im.h, x, fh as i32 / 2 + 16 * k as i32);
        x += im.w as i32 + 16 * k as i32;
    }

    // What one blit costs, into a scratch buffer so the picture stays.
    let path = if draw::blend::has_avx2() { "avx2" } else { "scalar" };
    for (im, name) in images.iter().zip(&paths) {
        let mut scratch = vec![SOLID; im.w * im.h];
        let mut sc = Canvas::new(&mut scratch, im.w, im.h, im.w);
        let t0 = now_us();
        for _ in 0..BLITS {
            sc.blit_over(core::hint::black_box(&im.px), im.w, im.h, 0, 0);
        }
        let ns = (now_us() - t0) * 1000 / BLITS as i64;
        println!("imgview: blit_over ({}) {} {}x{}: {} ns per blit", path, name, im.w, im.h, ns);
    }

    // A whole frame of translucent pixels (the first image tiled), blended
    // row by row: scalar against the dispatched path.
    let im = &images[0];
    let layer: Vec<u32> = (0..fw * fh).map(|i| im.px[(i / fw % im.h) * im.w + i % fw % im.w]).collect();
    let mut scratch = vec![SOLID; fw * fh];
    for (name, f) in [
        ("scalar", draw::blend::over_row_scalar as fn(&mut [u32], &[u32])),
        (path, draw::blend::over_row),
    ] {
        let t0 = now_us();
        for _ in 0..10 {
            for (d, s) in scratch.chunks_exact_mut(fw).zip(layer.chunks_exact(fw)) {
                f(d, core::hint::black_box(s));
            }
        }
        println!("imgview: full frame {}x{} ({}): {} us per frame", fw, fh, name, (now_us() - t0) / 10);
    }

    gfx.present(&frame);
    println!("imgview: ready");
    loop {
        while let Some(ev) = gfx.next_event() {
            if ev.kind == EV_KEY && ev.value == 1 && (ev.code == KEY_ESC || ev.code == KEY_Q) {
                drop(gfx);
                return 0;
            }
        }
        gfx.present(&frame);
        syscall::sleep_ms(50);
    }
}
