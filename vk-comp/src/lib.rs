//! The GPU compositor (G5 layer 4, docs/gpu/g5-graphics-stack-plan.md): the machine around the window manager (`gui::compositor::Compositor`),
//! in Rust with std on musl. It owns the sockets (`/tmp/gui-0`), the keyboard and the mouse, the clients' programs and the window titles, and hands
//! every frame to the renderer (`probes/nvk/comp_vk.c`: C on NVK) as a list of rectangles that show a colour or pixels. The renderer imports the
//! GPU buffers its clients export where they are and draws with a graphics pipeline; it presents through the WSI's direct path (the display).
//!
//!   vk_comp [prog...]        starts each prog once the socket listens (default: panel, if it exists); Ctrl+Alt+Backspace quits
//!   COMP_HEADLESS=1          no display (QEMU's software device): render 640x360 without a screen
//!   COMP_NO_INPUT=1          do not open (or grab) the input devices
//!   COMP_SECONDS=<n>         quit after n seconds (unattended runs); SIGTERM quits too
//!   COMP_SOCKET=<path>       the socket (default /tmp/gui-0)
//!
//! Exported as C's `main`: build.py links this static library with NVK and the renderer.

mod ffi;
mod sys;
mod text_util;
mod titles;

use std::collections::BTreeMap;
use std::ffi::{c_char, c_int, CStr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gui::compositor::{scale_for, ClientId, Compositor, DrawOp, GpuOp, CURSOR, CURSOR_H, CURSOR_W};
use gui::wire::Encoder;

use crate::ffi::*;
use crate::sys::*;
use crate::titles::Titles;

static QUIT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_sig: c_int) {
    QUIT.store(true, Ordering::Relaxed);
}

/// Source keys of the renderer: a window is `client << 32 | surface`; a title is this plus its toplevel id; the cursor is this alone.
const TITLE_KEY: u64 = 1 << 62;
const CURSOR_KEY: u64 = 1 << 63;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;

struct Clients {
    streams: BTreeMap<ClientId, UnixStream>,
    seen: u32,
}

impl Clients {
    fn drop_client(&mut self, comp: &mut Compositor<Mapping>, c: ClientId) {
        if self.streams.remove(&c).is_some() {
            comp.remove_client(c);
            println!("COMP client {} gone", c);
        }
    }

    /// Carries out what the window manager queued: events to the clients (never blocking on one: a client that does not read is dropped), the
    /// disconnects it asked for, the descriptors it is done with.
    fn flush(&mut self, comp: &mut Compositor<Mapping>) {
        let mut per_client: BTreeMap<ClientId, Encoder> = BTreeMap::new();
        for (c, ev) in comp.take_events() {
            ev.encode(per_client.entry(c).or_default());
        }
        let mut dead = Vec::new();
        for (c, mut e) in per_client {
            let Some(s) = self.streams.get(&c) else { continue };
            let (bytes, _) = e.take();
            let n = unsafe { send(s.as_raw_fd(), bytes.as_ptr() as *const _, bytes.len(), MSG_DONTWAIT | MSG_NOSIGNAL) };
            if n != bytes.len() as i64 {
                // a client that closed its end is just leaving (its socket reads EOF next); one that left the socket full is not reading
                let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                if errno != 32 && errno != 104 {
                    println!("COMP client {} is not reading its events: dropped", c);
                }
                dead.push(c);
            }
        }
        for c in comp.take_disconnects() {
            println!("COMP client {} disconnected for a protocol error", c);
            dead.push(c);
        }
        for c in dead {
            self.drop_client(comp, c);
        }
        for fd in comp.take_fds_to_close() {
            unsafe { close(fd) };
        }
    }

    /// Everything a client sent since the last time.
    fn readable(&mut self, comp: &mut Compositor<Mapping>, c: ClientId) {
        let Some(fd) = self.streams.get(&c).map(|s| s.as_raw_fd()) else { return };
        let mut buf = vec![0u8; 4096];
        loop {
            match recv_with_fds(fd, &mut buf) {
                Ok((0, fds)) if fds.is_empty() => return self.drop_client(comp, c),
                Ok((n, fds)) => comp.client_data(c, &buf[..n], &fds, &mut map_pool),
                Err(4) => continue, // EINTR
                Err(_) => return,   // EAGAIN: all read
            }
            self.flush(comp);
            if !self.streams.contains_key(&c) {
                return;
            }
        }
    }
}

/// An evdev device, one 24-byte `struct input_event` per read.
fn read_input(fd: i32, mut f: impl FnMut(u16, u16, i32)) {
    let mut rec = [0u8; 24];
    for _ in 0..256 {
        if unsafe { read(fd, rec.as_mut_ptr() as *mut _, 24) } != 24 {
            return;
        }
        let ty = u16::from_ne_bytes([rec[16], rec[17]]);
        let code = u16::from_ne_bytes([rec[18], rec[19]]);
        let value = i32::from_ne_bytes([rec[20], rec[21], rec[22], rec[23]]);
        f(ty, code, value);
    }
}

fn open_input(path: &str) -> i32 {
    let c = std::ffi::CString::new(path).unwrap();
    unsafe { open(c.as_ptr(), O_RDONLY | O_NONBLOCK) }
}

/// Starts `cmd` (a program name looked up in /bin and /mnt/bin, or a path; arguments split on spaces) in a group of its own.
fn start_program(cmd: &str, socket: &str) -> Option<u32> {
    let mut parts = cmd.split_whitespace();
    let name = parts.next()?;
    let path = if name.contains('/') {
        name.to_string()
    } else if std::path::Path::new(&format!("/bin/{name}")).exists() {
        format!("/bin/{name}")
    } else {
        format!("/mnt/bin/{name}")
    };
    if !std::path::Path::new(&path).exists() {
        return None;
    }
    let mut c = Command::new(&path);
    c.args(parts).env("GUI_DISPLAY", socket).process_group(0);
    unsafe {
        c.pre_exec(|| {
            // what the compositor ignores (a ^C typed into a window is that window's) its children get back
            signal(SIGINT, SIG_DFL);
            signal(SIGTSTP, SIG_DFL);
            signal(SIGTERM, SIG_DFL);
            Ok(())
        });
    }
    c.spawn().ok().map(|ch| ch.id())
}

fn cursor_pixels() -> Vec<u32> {
    let mut px = vec![0u32; (CURSOR_W * CURSOR_H) as usize];
    for (y, row) in CURSOR.iter().enumerate() {
        for (x, c) in row.iter().enumerate() {
            px[y * CURSOR_W as usize + x] = match c {
                b'X' => 0xff00_0000,
                b'.' => 0xffff_ffff,
                _ => 0,
            };
        }
    }
    px
}

fn run(args: &[String]) -> i32 {
    unsafe {
        signal(SIGINT, SIG_IGN); // a ^C or ^Z typed into a window is for that window
        signal(SIGTSTP, SIG_IGN);
        signal(SIGPIPE, SIG_IGN);
        signal(SIGTERM, on_term as *const () as usize);
    }
    let env = |k: &str| std::env::var(k).ok();
    let headless = env("COMP_HEADLESS").is_some();
    let no_input = env("COMP_NO_INPUT").is_some();
    let seconds: u64 = env("COMP_SECONDS").and_then(|s| s.parse().ok()).unwrap_or(0);
    let socket = env("COMP_SOCKET").unwrap_or_else(|| "/tmp/gui-0".into());

    let (mut w, mut h) = (0u32, 0u32);
    if unsafe { cr_init(headless as c_int, &mut w, &mut h) } != 0 {
        println!("COMP FAILED (1)");
        unsafe { cr_shutdown() };
        return 1;
    }
    let mut comp: Compositor<Mapping> = Compositor::new(w as i32, h as i32);
    comp.enable_gpu_buffers();
    let mut titles = Titles::new(scale_for(h as i32));
    if titles.missing_fonts() > 0 {
        println!("COMP {} font files missing, titles in the bitmap font", titles.missing_fonts());
    }
    let cursor = cursor_pixels();

    let _ = std::fs::remove_file(&socket); // a dead compositor's
    let listener = match UnixListener::bind(&socket) {
        Ok(l) => l,
        Err(e) => {
            println!("COMP FAIL cannot listen on {} ({})", socket, e);
            unsafe { cr_shutdown() };
            println!("COMP FAILED (1)");
            return 1;
        }
    };
    listener.set_nonblocking(true).ok();
    let (mut kbd, mut mouse) = (-1, -1);
    if !no_input {
        kbd = open_input("/dev/input/event0");
        mouse = open_input("/dev/input/event1");
        if kbd < 0 || mouse < 0 {
            println!("COMP FAIL cannot open the input devices ({} {})", kbd, mouse);
            unsafe { cr_shutdown() };
            println!("COMP FAILED (1)");
            return 1;
        }
        unsafe { ioctl(kbd, EVIOCGRAB, 1) };
        read_input(kbd, |_, _, _| {}); // what the rings hold from before is not ours
        read_input(mouse, |_, _, _| {});
    }
    println!("COMP listening on {}", socket);
    if args.len() > 1 {
        for cmd in &args[1..] {
            match start_program(cmd, &socket) {
                Some(pid) => println!("COMP started {} (pid {})", cmd, pid),
                None => println!("COMP cannot start {}", cmd),
            }
        }
    } else if std::path::Path::new("/bin/panel").exists() || std::path::Path::new("/mnt/bin/panel").exists() {
        start_program("panel", &socket);
    }

    let mut clients = Clients { streams: BTreeMap::new(), seen: 0 };
    let t_start = Instant::now();
    let mut last_stat = Instant::now();
    let (mut mdx, mut mdy) = (0i32, 0i32);
    let mut frames = 0u64;
    let mut failures = 0;
    let uptime_ms = || t_start.elapsed().as_millis() as u32;

    while !QUIT.load(Ordering::Relaxed) && !comp.quit_requested() {
        if seconds > 0 && t_start.elapsed() >= Duration::from_secs(seconds) {
            break;
        }
        let busy = comp.has_damage() || comp.has_frame_callbacks();
        let mut pfds = vec![PollFd { fd: listener.as_raw_fd(), events: POLLIN, revents: 0 }];
        let mut who: Vec<u32> = vec![0];
        if kbd >= 0 {
            pfds.push(PollFd { fd: kbd, events: POLLIN, revents: 0 });
            who.push(u32::MAX - 1);
            pfds.push(PollFd { fd: mouse, events: POLLIN, revents: 0 });
            who.push(u32::MAX - 2);
        }
        for (c, s) in &clients.streams {
            pfds.push(PollFd { fd: s.as_raw_fd(), events: POLLIN, revents: 0 });
            who.push(*c);
        }
        unsafe { poll(pfds.as_mut_ptr(), pfds.len() as u64, if busy { 0 } else { 200 }) };
        comp.set_time(uptime_ms());
        while unsafe { waitpid(-1, std::ptr::null_mut(), WNOHANG) } > 0 {} // what we started and has exited
        for (p, id) in pfds.iter().zip(&who) {
            if p.revents & (POLLIN | POLLHUP) == 0 {
                continue;
            }
            match *id {
                0 => {
                    while let Ok((s, _)) = listener.accept() {
                        s.set_nonblocking(true).ok();
                        let c = comp.add_client();
                        clients.streams.insert(c, s);
                        clients.seen += 1;
                        println!("COMP client {} connected", c);
                    }
                }
                x if x == u32::MAX - 1 => read_input(kbd, |ty, code, value| {
                    if ty == EV_KEY {
                        comp.key(code as u32, value != 0);
                    }
                }),
                x if x == u32::MAX - 2 => read_input(mouse, |ty, code, value| match (ty, code) {
                    (EV_REL, REL_X) => mdx += value,
                    (EV_REL, REL_Y) => mdy -= value, // PS/2: positive is up; the screen's is down
                    (EV_KEY, _) => comp.pointer_button(code as u32, value != 0),
                    (EV_SYN, _) => {
                        if mdx != 0 || mdy != 0 {
                            comp.pointer_motion(mdx, mdy);
                        }
                        mdx = 0;
                        mdy = 0;
                    }
                    _ => {}
                }),
                c => clients.readable(&mut comp, c),
            }
        }
        clients.flush(&mut comp);

        if comp.has_damage() || comp.has_frame_callbacks() {
            // the previous frame is done: the buffers it read may be released to their clients, and what was dropped may go
            let done = unsafe { cr_wait() };
            if done != 0 {
                comp.gpu_frame_done(done);
            }
            clients.flush(&mut comp);
            for op in comp.take_gpu_ops() {
                match op {
                    GpuOp::Import { handle, fd, size, stride, .. } => unsafe {
                        // the renderer takes the descriptor (closes it), whether or not the import works
                        cr_import(handle, fd, size as u64, stride as u32);
                    },
                    GpuOp::Drop { handle } => unsafe { cr_drop(handle) },
                }
            }
            let (epoch, list) = comp.draw_list();
            let mut ops: Vec<CrOp> = Vec::with_capacity(list.len());
            let mut live_titles = Vec::new();
            for op in &list {
                match op {
                    DrawOp::Fill { rect, color } => {
                        ops.push(CrOp { color: *color, x: rect.x, y: rect.y, w: rect.w, h: rect.h, ..CrOp::new(CR_FILL) });
                    }
                    DrawOp::Gpu { handle, dst, sx, sy } => {
                        ops.push(CrOp { key: *handle, x: dst.x, y: dst.y, w: dst.w, h: dst.h, sx: *sx, sy: *sy, ..CrOp::new(CR_GPU) });
                    }
                    DrawOp::Cpu { client, surface, version, dst, sx, sy, w: sw, .. } => {
                        if let Some(px) = comp.cpu_content(*client, *surface) {
                            ops.push(CrOp {
                                key: ((*client as u64) << 32) | *surface as u64,
                                version: *version,
                                px: px.as_ptr(),
                                npx: px.len() as u64,
                                src_w: *sw,
                                x: dst.x,
                                y: dst.y,
                                w: dst.w,
                                h: dst.h,
                                sx: *sx,
                                sy: *sy,
                                ..CrOp::new(CR_CPU)
                            });
                        }
                    }
                    DrawOp::Title { id, title, focused, area, clip } => {
                        live_titles.push(*id);
                        let img = titles.image(*id, title, *focused, area.w, area.h);
                        if img.px.is_empty() {
                            continue;
                        }
                        ops.push(CrOp {
                            key: TITLE_KEY + *id as u64,
                            version: img.version,
                            px: img.px.as_ptr(),
                            npx: img.px.len() as u64,
                            src_w: img.w,
                            x: clip.x,
                            y: clip.y,
                            w: clip.w,
                            h: clip.h,
                            sx: clip.x - area.x,
                            sy: clip.y - area.y,
                            ..CrOp::new(CR_CPU)
                        });
                    }
                    DrawOp::Cursor { x, y } => {
                        ops.push(CrOp {
                            key: CURSOR_KEY,
                            version: 1,
                            px: cursor.as_ptr(),
                            npx: cursor.len() as u64,
                            src_w: CURSOR_W,
                            keyed: 1,
                            x: *x,
                            y: *y,
                            w: CURSOR_W,
                            h: CURSOR_H,
                            ..CrOp::new(CR_CPU)
                        });
                    }
                }
            }
            titles.retain(|id| live_titles.contains(&id));
            if unsafe { cr_frame(ops.as_ptr(), ops.len(), epoch) } != 0 {
                println!("COMP FAIL frame {}", frames);
                failures += 1;
                break;
            }
            frames += 1;
            // The frame is on the screen once its flip has landed, at the next vblank: only then do its clients hear it (the `frame` callbacks), so they
            // draw their next frame in the interval and every client's commit is in the next composition. Composing as soon as one commit arrives
            // would give each vblank to one client.
            unsafe { cr_wait_flip() };
            comp.frame_done(uptime_ms());
            clients.flush(&mut comp);
        }
        if last_stat.elapsed() >= Duration::from_secs(5) {
            last_stat = Instant::now();
            let mut st = CrStats::default();
            unsafe { cr_get_stats(&mut st) };
            println!("COMP {} frames, {} draws in the last, {} imports, {} uploads, {} clients seen", frames, st.draws, st.imports, st.uploads, clients.seen);
        }
    }
    let mut st = CrStats::default();
    unsafe { cr_get_stats(&mut st) };
    println!("COMP quit after {} frames (up to {} draws), {} imports, {} drops, {} uploads, {} clients seen", frames, st.draws_max, st.imports, st.drops, st.uploads, clients.seen);

    let ids: Vec<ClientId> = clients.streams.keys().copied().collect();
    for c in ids {
        clients.drop_client(&mut comp, c);
    }
    drop(listener);
    let _ = std::fs::remove_file(&socket);
    if kbd >= 0 {
        drop(unsafe { OwnedFd::from_raw_fd(kbd) });
        drop(unsafe { OwnedFd::from_raw_fd(mouse) });
    }
    unsafe { cr_shutdown() };
    if failures > 0 {
        println!("COMP FAILED ({})", failures);
        return 1;
    }
    println!("COMP DONE");
    0
}

/// C's `main`: Rust's own startup does not run, so the arguments are `argc` and `argv`.
#[no_mangle]
pub extern "C" fn main(argc: c_int, argv: *const *const c_char) -> c_int {
    let args: Vec<String> = (0..argc.max(0) as usize).map(|i| unsafe { CStr::from_ptr(*argv.add(i)) }.to_string_lossy().into_owned()).collect();
    run(&args)
}
