#![no_std]
#![no_main]

//! A client of the compositor (phase 2.5 of `docs/gui/gui-plan.md`): one
//! window with an animated gradient, one frame per `frame` callback, and
//! every key, click and motion it receives printed to its own stdout.
//!
//! `gui_demo [width height]` (default 320x200). Exits when the compositor
//! goes away or on Esc.

extern crate alloc;

use gui::protocol::{Event, Interface, Request, FORMAT_XRGB8888};
use gui::wire::{Decoder, Encoder};
use userspace::args::Args;
use userspace::syscall::{self, AF_UNIX, MAP_SHARED, PROT_READ, PROT_WRITE, SOCK_STREAM};
use userspace::{entry, println};

entry!(main);

const POOL: u32 = 2;
const BUFFER: u32 = 3;
const SURFACE: u32 = 4;
/// Callback ids count up from here; ids below are the fixed objects.
const FIRST_CALLBACK: u32 = 16;
const KEY_ESC: u32 = 1;

fn parse(b: Option<&[u8]>, default: i32) -> i32 {
    let Some(b) = b else { return default };
    let mut n = 0i32;
    for &d in b {
        if !d.is_ascii_digit() {
            return default;
        }
        n = n.saturating_mul(10).saturating_add((d - b'0') as i32);
    }
    if n > 0 { n.min(2048) } else { default }
}

fn send(fd: i32, reqs: &[Request]) -> bool {
    let mut e = Encoder::new();
    for r in reqs {
        r.encode(&mut e);
    }
    let (bytes, fds) = e.take();
    syscall::send_fds(fd, &bytes, &fds, 0) == bytes.len() as i64
}

fn draw(px: &mut [u32], w: i32, h: i32, t: u32) {
    for y in 0..h {
        for x in 0..w {
            let r = (x as u32 + t) & 0xFF;
            let g = (y as u32 + 2 * t) & 0xFF;
            let b = 0x80 + ((x as u32 ^ y as u32) & 0x3F);
            px[(y * w + x) as usize] = r << 16 | g << 8 | b;
        }
    }
}

fn main(args: Args) -> i32 {
    let w = parse(args.get(1), 320);
    let h = parse(args.get(2), 200);

    // The compositor may still be starting: retry for two seconds.
    let fd = syscall::socket(AF_UNIX as i32, SOCK_STREAM, 0) as i32;
    let (addr, alen) = syscall::SockAddrUn::path(b"/tmp/gui-0");
    let mut tries = 0;
    while syscall::connect(fd, &addr, alen) < 0 {
        tries += 1;
        if tries > 40 {
            println!("gui_demo: no compositor at /tmp/gui-0");
            return 1;
        }
        syscall::sleep_ms(50);
    }

    let size = (w * h * 4) as u64;
    let mfd = syscall::memfd_create(b"gui_demo\0", 0) as i32;
    let base = if mfd >= 0 && syscall::ftruncate(mfd, size) == 0 {
        syscall::mmap(0, size, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0)
    } else {
        -1
    };
    if base <= 0 {
        println!("gui_demo: cannot create the pool");
        return 1;
    }
    let px = unsafe { core::slice::from_raw_parts_mut(base as *mut u32, (w * h) as usize) };

    let mut t = 0u32;
    let mut cb = FIRST_CALLBACK;
    draw(px, w, h, t);
    let ok = send(fd, &[
        Request::CreatePool { id: POOL, fd: mfd, size: size as u32 },
        Request::CreateBuffer { pool: POOL, id: BUFFER, offset: 0, width: w, height: h, stride: w * 4, format: FORMAT_XRGB8888 },
        Request::CreateSurface { id: SURFACE },
        Request::SetTitle { surface: SURFACE, title: "gui_demo".into() },
        Request::Attach { surface: SURFACE, buffer: BUFFER },
        Request::Damage { surface: SURFACE, x: 0, y: 0, w, h },
        Request::Frame { surface: SURFACE, id: cb },
        Request::Commit { surface: SURFACE },
    ]);
    syscall::close(mfd); // the compositor has its own now
    if !ok {
        println!("gui_demo: send failed");
        return 1;
    }
    println!("gui_demo: {}x{} window up", w, h);

    let mut dec = Decoder::new();
    let mut buf = [0u8; 1024];
    let mut released = false;
    let mut frames = 0u32;
    let t0 = syscall::uptime_ms();
    loop {
        let n = syscall::recv(fd, &mut buf);
        if n <= 0 {
            println!("gui_demo: compositor gone after {} frames", frames);
            return 0;
        }
        dec.push_bytes(&buf[..n as usize]);
        while let Ok(Some(msg)) = dec.next_message() {
            let iface = match msg.object {
                1 => Interface::Compositor,
                BUFFER => Interface::Buffer,
                SURFACE => Interface::Surface,
                _ => Interface::Callback,
            };
            let Ok(ev) = Event::decode(iface, &msg) else {
                println!("gui_demo: undecodable event on object {}", msg.object);
                continue;
            };
            match ev {
                Event::Release { .. } => released = true,
                Event::Done { callback, ms } if callback == cb => {
                    frames += 1;
                    if frames % 120 == 0 {
                        let secs = (syscall::uptime_ms() - t0).max(1);
                        println!("gui_demo: {} frames, {} fps (compositor clock {} ms)", frames, frames as i64 * 1000 / secs, ms);
                    }
                    if !released {
                        continue; // cannot happen: release precedes done
                    }
                    t = t.wrapping_add(2);
                    draw(px, w, h, t);
                    released = false;
                    cb += 1;
                    if !send(fd, &[
                        Request::Attach { surface: SURFACE, buffer: BUFFER },
                        Request::Damage { surface: SURFACE, x: 0, y: 0, w, h },
                        Request::Frame { surface: SURFACE, id: cb },
                        Request::Commit { surface: SURFACE },
                    ]) {
                        // The compositor closed between its last event and
                        // this send (EPIPE): the same ending as an EOF.
                        println!("gui_demo: compositor gone after {} frames", frames);
                        return 0;
                    }
                }
                Event::Key { code, pressed, .. } => {
                    println!("gui_demo: key {} {}", code, if pressed { "down" } else { "up" });
                    if code == KEY_ESC && pressed {
                        println!("gui_demo: Esc, bye after {} frames", frames);
                        return 0;
                    }
                }
                Event::Button { code, pressed, .. } => {
                    println!("gui_demo: button {:#x} {}", code, if pressed { "down" } else { "up" })
                }
                Event::Motion { x, y, .. } => println!("gui_demo: motion {},{}", x, y),
                Event::Focus { focused, .. } => println!("gui_demo: focus {}", if focused { "in" } else { "out" }),
                Event::Configure { width, height, .. } => println!("gui_demo: configure {}x{}", width, height),
                Event::Error { object, code, message } => {
                    println!("gui_demo: error {} on {}: {}", code, object, message);
                    return 1;
                }
                Event::DeleteId { .. } | Event::Done { .. } => {}
            }
        }
    }
}
