//! The smallest Rust std program with a window (`gui_client`): a gradient with a square that follows the pointer, every event printed to
//! stdout (`scripts/gui-e2e.sh std` reads them from the serial log). Esc or the close button ends it.
//!
//!     compositor /mnt/bin/hello-window

use std::process::ExitCode;

use gui_client::{Event, Window};

const KEY_ESC: u32 = 1;

fn draw(px: &mut [u32], w: usize, h: usize, at: Option<(i32, i32)>, pressed: bool) {
    for y in 0..h {
        for x in 0..w {
            let r = (x * 255 / w.max(1)) as u32;
            let b = (y * 255 / h.max(1)) as u32;
            px[y * w + x] = r << 16 | 0x40 << 8 | b;
        }
    }
    if let Some((cx, cy)) = at {
        let c = if pressed { 0x00ff_ff00 } else { 0x00ff_ffff };
        for y in cy - 8..cy + 8 {
            for x in cx - 8..cx + 8 {
                if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                    px[y as usize * w + x as usize] = c;
                }
            }
        }
    }
}

fn run() -> std::io::Result<()> {
    let mut win = Window::open("hello-window", Some((320, 200)))?;
    win.set_resizable(64, 48)?;
    let (mut at, mut pressed) = (None, false);
    let (w, h) = win.size();
    draw(win.frame()?, w, h, at, pressed);
    win.present()?;
    println!("hello-window: ready {}x{}", w, h);
    loop {
        let Some(ev) = win.next_event(None)? else { continue };
        match ev {
            Event::Key { code, pressed } => {
                println!("hello-window: key {} {}", code, if pressed { "down" } else { "up" });
                if code == KEY_ESC && pressed {
                    break;
                }
            }
            Event::Motion { x, y } => {
                println!("hello-window: motion {} {}", x, y);
                at = Some((x, y));
            }
            Event::Button { code, pressed: p } => {
                println!("hello-window: button {:#x} {}", code, if p { "down" } else { "up" });
                pressed = p;
            }
            Event::RelativeMotion { dx, dy } => println!("hello-window: relative {} {}", dx, dy),
            Event::Focus(f) => println!("hello-window: focus {}", if f { "in" } else { "out" }),
            Event::Resize { width, height } => {
                win.resize(width, height)?;
                println!("hello-window: resized to {}x{}", width, height);
            }
            Event::Close => break,
        }
        let (w, h) = win.size();
        draw(win.frame()?, w, h, at, pressed);
        win.present()?;
    }
    println!("hello-window: bye");
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hello-window: {}", e);
            ExitCode::FAILURE
        }
    }
}
