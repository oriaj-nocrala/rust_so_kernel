//! The GPU compositor (G5 layer 4, docs/gpu/g5-graphics-stack-plan.md): the machine around the window manager (`gui::compositor::Compositor`),
//! in Rust with std on musl. It owns the sockets (`/tmp/gui-0`), the keyboard and the mouse, the clients' programs and the window titles, and hands
//! every frame to the renderer (`probes/nvk/comp_vk.c`: C on NVK) as a list of rectangles that show a colour or pixels. The renderer imports the
//! GPU buffers its clients export where they are and draws with a graphics pipeline; it presents through the WSI's direct path (the display).
//!
//!   vk_comp [prog...]        starts each prog once the socket listens (default: panel, if it exists); Ctrl+Alt+Backspace quits
//!   COMP_HEADLESS=1          no display (QEMU's software device): render 640x360 without a screen
//!   COMP_NO_INPUT=1          do not open (or grab) the input devices
//!   COMP_SECONDS=<n>         quit after n seconds (unattended runs); SIGTERM quits too. The programs it started that are still running then are
//!                            sent SIGTERM (SIGKILL after 3 s), so a session ends with its compositor
//!   COMP_NO_PANEL=1          with no program to start, do not start the default panel
//!   COMP_F11_AT=60,120       test hook: press F11 (fullscreen on the focused window) when that many frames have been composed
//!   COMP_THEME=<name>        the look to start with: luna (default), 9x or flat (gui::theme); F12 cycles them
//!   COMP_EXIT_WHEN_IDLE=1    quit when every client that connected has gone and every program it started has exited (the quit line then
//!                            says how long it all took)
//!
//! Every 5 s (and at the end) a `COMP pace` line says where each composition's time went and how far apart its flips landed.
//!   COMP_SOCKET=<path>       the socket (default /tmp/gui-0)
//!   COMP_DELAY_MS=<n>        how long after a frame is on the screen the next one is composed (default 2). Its PRESENT must go out within
//!                            ~9-11 ms of the previous flip being seen or it lands a vblank late (Ryzen #181); the clients draw after the
//!                            previous composition's present, so their commits are already in by then
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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
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

/// Programs not started yet, and the ones started and not reaped (their pids).
static LAUNCHING: AtomicUsize = AtomicUsize::new(0);
static CHILDREN: Mutex<Vec<i32>> = Mutex::new(Vec::new());

/// Starts `cmds` on a thread of their own. `spawn` returns only once the child's exec has loaded the program, which takes seconds for a 15 MB
/// static binary the block cache no longer holds; the compositor must keep answering the clients already running meanwhile (one waits 5 s for
/// its first `configure` and then gives up: every client of three runs of Ryzen #180 left before the first frame).
fn launch(cmds: Vec<String>, socket: String) {
    LAUNCHING.store(cmds.len(), Ordering::SeqCst);
    std::thread::spawn(move || {
        for cmd in cmds {
            let t = Instant::now();
            match start_program(&cmd, &socket) {
                Some(pid) => {
                    CHILDREN.lock().unwrap().push(pid as i32);
                    println!("COMP started {} (pid {}, {} ms)", cmd, pid, t.elapsed().as_millis());
                }
                None => println!("COMP cannot start {}", cmd),
            }
            LAUNCHING.fetch_sub(1, Ordering::SeqCst); // after the push: "nothing launching and no children" is never seen in between
        }
    });
}

/// Reaps the programs we started that have exited (by pid: a `waitpid(-1)` could take a child whose exec failed from under `spawn`, which
/// waits for it itself). Whether any is still to start or running.
fn reap_children() -> bool {
    let mut children = CHILDREN.lock().unwrap();
    children.retain(|&pid| {
        let mut status = 0;
        let r = unsafe { waitpid(pid, &mut status, WNOHANG) };
        if r == pid {
            if status & 0x7f == 0 {
                println!("COMP program pid {} exited ({})", pid, (status >> 8) & 0xff);
            } else {
                println!("COMP program pid {} killed by signal {}", pid, status & 0x7f);
            }
        }
        r == 0
    });
    !children.is_empty() || LAUNCHING.load(Ordering::SeqCst) > 0
}

/// The programs we started that are still running when the compositor ends go with it, as a session's do: SIGTERM (their handlers were put back
/// to the default when they started), SIGKILL after 3 s. How many had to be told.
fn terminate_children() -> usize {
    let pids: Vec<i32> = CHILDREN.lock().unwrap().clone();
    for &p in &pids {
        unsafe { kill(p, SIGTERM) };
    }
    let t = Instant::now();
    while !pids.is_empty() && reap_children() && t.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(50));
    }
    for p in CHILDREN.lock().unwrap().clone() {
        unsafe { kill(p, SIGKILL) };
    }
    if !pids.is_empty() {
        reap_children();
    }
    pids.len()
}

/// Where the time of each composition goes, and how far apart its flips land, per 5 s window: the display is 60 Hz, so a flip every ~16.7 ms
/// is a composition per vblank and ~33 ms means one vblank missed.
#[derive(Default)]
struct Pace {
    n: u64,
    sum: [u64; PACE_PHASES],
    max: [u64; PACE_PHASES],
    /// Flip-to-flip intervals: under 20 ms, 20-37 ms, over 37 ms.
    gaps: [u64; 3],
    /// When the PRESENT went out, from the previous flip landing: the latest that still made the next vblank and the earliest that missed it
    /// (the real deadline is between them).
    hit_max: u64,
    miss_min: u64,
}

const PACE_PHASES: usize = 8;
const PACE_NAMES: [&str; PACE_PHASES] = ["late", "wait", "build", "acquire", "render", "present", "flip", "interval"];

impl Pace {
    /// `us`: microseconds of each phase (`PACE_NAMES`): "late" is from the previous flip landing to this composition starting, "interval" from
    /// that flip landing to this one's (0 for the first frame).
    fn add(&mut self, us: [u64; PACE_PHASES]) {
        self.n += 1;
        for i in 0..PACE_PHASES {
            self.sum[i] += us[i];
            self.max[i] = self.max[i].max(us[i]);
        }
        let iv = us[PACE_PHASES - 1];
        if iv > 0 {
            self.gaps[if iv < 20_000 { 0 } else if iv <= 37_000 { 1 } else { 2 }] += 1;
            let sent = us[..6].iter().sum::<u64>(); // late + wait + build + acquire + render + present
            if iv < 20_000 {
                self.hit_max = self.hit_max.max(sent);
            } else if iv <= 37_000 && (self.miss_min == 0 || sent < self.miss_min) {
                self.miss_min = sent;
            }
        }
    }

    fn line(&self) -> String {
        let ms = |v: u64| format!("{}.{}", v / 1000, v % 1000 / 100);
        let mut s = format!(
            "{} frames, flips <20ms {} 20-37ms {} >37ms {}; PRESENT after the flip: latest on time {} earliest late {}; avg/max ms:",
            self.n,
            self.gaps[0],
            self.gaps[1],
            self.gaps[2],
            ms(self.hit_max),
            if self.miss_min == 0 { "-".into() } else { ms(self.miss_min) }
        );
        for i in 0..PACE_PHASES {
            let avg = self.sum[i] / self.n.max(1);
            s += &format!(" {} {}.{}/{}.{}", PACE_NAMES[i], avg / 1000, avg % 1000 / 100, self.max[i] / 1000, self.max[i] % 1000 / 100);
        }
        s
    }
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
    let exit_when_idle = env("COMP_EXIT_WHEN_IDLE").is_some();
    let compose_delay = Duration::from_millis(env("COMP_DELAY_MS").and_then(|s| s.parse().ok()).unwrap_or(2));
    // A test hook: F11 is pressed (on the focused window) when this many frames have been composed, once per number in the list
    // (COMP_F11_AT=60,120), so QEMU, which has no keyboard for the compositor to read, can take a window to fullscreen and back.
    let f11_at: Vec<u64> = env("COMP_F11_AT").map(|l| l.split(',').filter_map(|n| n.trim().parse().ok()).collect()).unwrap_or_default();

    let (mut w, mut h) = (0u32, 0u32);
    if unsafe { cr_init(headless as c_int, &mut w, &mut h) } != 0 {
        println!("COMP FAILED (1)");
        unsafe { cr_shutdown() };
        return 1;
    }
    let mut comp: Compositor<Mapping> = Compositor::new(w as i32, h as i32);
    comp.enable_gpu_buffers();
    let theme_name = env("COMP_THEME").unwrap_or_else(|| "luna".into());
    comp.set_theme(gui::theme::by_name(&theme_name).unwrap_or_else(|| {
        println!("COMP no theme {:?}: luna", theme_name);
        &gui::theme::LUNA
    }));
    let mut theme_shown = "";
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
        launch(args[1..].to_vec(), socket.clone());
    } else if env("COMP_NO_PANEL").is_none() && (std::path::Path::new("/bin/panel").exists() || std::path::Path::new("/mnt/bin/panel").exists()) {
        launch(vec!["panel".into()], socket.clone());
    }

    let mut clients = Clients { streams: BTreeMap::new(), seen: 0 };
    let t_start = Instant::now();
    let mut last_stat = Instant::now();
    let (mut mdx, mut mdy) = (0i32, 0i32);
    let mut frames = 0u64;
    // what the input devices delivered (is the mouse alive?): key events, pointer motions, button events
    let (mut n_keys, mut n_moves, mut n_buttons) = (0u64, 0u64, 0u64);
    // the mouse, closer: poll() said it was readable, EV_REL records read, the sum of the deltas
    let (mut n_mouse_wakes, mut n_rel, mut sum_dx, mut sum_dy) = (0u64, 0u64, 0i64, 0i64);
    let mut failures = 0;
    // No composition before this instant: a frame is composed once per vblank with whatever every client committed since the last one, not as soon as
    // the first commit arrives (that gave each vblank to one client, and each of two clients 30 frames per second).
    let mut compose_not_before = Instant::now();
    let uptime_ms = || t_start.elapsed().as_millis() as u32;
    let (mut pace, mut pace_all) = (Pace::default(), Pace::default());
    let mut last_flip: Option<Instant> = None;
    let mut programs_alive = true;
    // The frame rate while there are clients: from the first frame composed with one connected to the last such frame (the programs' start, a
    // 15 MB exec each, is not the compositor's rate).
    let mut with_clients: Option<(Instant, Instant, u64)> = None;

    while !QUIT.load(Ordering::Relaxed) && !comp.quit_requested() {
        if seconds > 0 && t_start.elapsed() >= Duration::from_secs(seconds) {
            break;
        }
        // idle: every program we were given has been started and has exited, and every client has gone (programs that are still loading
        // have not connected yet: the first client to finish is not the end)
        if exit_when_idle && clients.seen > 0 && clients.streams.is_empty() && !programs_alive {
            break;
        }
        let busy = comp.has_damage() || comp.has_frame_callbacks();
        let wait_ms = if busy { compose_not_before.saturating_duration_since(Instant::now()).as_millis() as c_int } else { 200 };
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
        unsafe { poll(pfds.as_mut_ptr(), pfds.len() as u64, wait_ms) };
        comp.set_time(uptime_ms());
        programs_alive = reap_children();
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
                        n_keys += 1;
                        comp.key(code as u32, value != 0);
                    }
                }),
                x if x == u32::MAX - 2 => {
                  n_mouse_wakes += 1;
                  read_input(mouse, |ty, code, value| match (ty, code) {
                    (EV_REL, REL_X) => {
                        n_rel += 1;
                        sum_dx += value as i64;
                        mdx += value
                    }
                    (EV_REL, REL_Y) => {
                        n_rel += 1;
                        sum_dy += value as i64;
                        mdy -= value // PS/2: positive is up; the screen's is down
                    }
                    (EV_KEY, _) => {
                        n_buttons += 1;
                        comp.pointer_button(code as u32, value != 0)
                    }
                    (EV_SYN, _) => {
                        if mdx != 0 || mdy != 0 {
                            n_moves += 1;
                            comp.pointer_motion(mdx, mdy);
                        }
                        mdx = 0;
                        mdy = 0;
                    }
                    _ => {}
                  })
                }
                c => clients.readable(&mut comp, c),
            }
        }
        clients.flush(&mut comp);

        if (comp.has_damage() || comp.has_frame_callbacks()) && Instant::now() >= compose_not_before {
            let t_compose = Instant::now();
            // the previous frame is done: the buffers it read may be released to their clients, and what was dropped may go
            let done = unsafe { cr_wait() };
            let t_waited = Instant::now();
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
                    DrawOp::Title { id, title, fg, shadow, area, clip, .. } => {
                        live_titles.push(*id);
                        let img = titles.image(*id, title, *fg, *shadow, area.w, area.h);
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
                            alpha: CR_PREMUL,
                            ..CrOp::new(CR_CPU)
                        });
                    }
                    DrawOp::Shape { rect, shape } => {
                        let shape = CrShape {
                            radius: shape.radius,
                            border: shape.border,
                            split: shape.split,
                            shadow_blur: shape.shadow_blur,
                            c: shape.c,
                            border_color: shape.border_color,
                            shadow_color: shape.shadow_color,
                            shadow_dx: shape.shadow_dx,
                            shadow_dy: shape.shadow_dy,
                            horizontal: shape.horizontal as u32,
                        };
                        ops.push(CrOp { x: rect.x, y: rect.y, w: rect.w, h: rect.h, shape, ..CrOp::new(CR_SHAPE) });
                    }
                    DrawOp::Cursor { x, y } => {
                        ops.push(CrOp {
                            key: CURSOR_KEY,
                            version: 1,
                            px: cursor.as_ptr(),
                            npx: cursor.len() as u64,
                            src_w: CURSOR_W,
                            alpha: CR_KEYED,
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
            if comp.theme().name != theme_shown {
                theme_shown = comp.theme().name;
                println!("COMP theme {}", theme_shown);
            }
            let t_built = Instant::now();
            if unsafe { cr_frame(ops.as_ptr(), ops.len(), epoch) } != 0 {
                println!("COMP FAIL frame {}", frames);
                failures += 1;
                break;
            }
            frames += 1;
            if f11_at.contains(&frames) {
                println!("COMP F11 at frame {}", frames);
                comp.key(87, true);
                comp.key(87, false);
            }
            let t_presented = Instant::now();
            // The clients whose commits are in this frame hear it now (the `frame` callbacks, as Weston sends them at repaint): the present above
            // waited for this frame's GPU work, so they draw their next frame while it waits for the vblank, and their commits are in the next
            // composition. Answering only once the flip had landed made their drawing overlap that next composition on the GPU (its present took
            // 5-6 ms instead of 1.3) and pushed its PRESENT past the vblank: 30 fps (Ryzen #181).
            comp.frame_done(uptime_ms());
            clients.flush(&mut comp);
            // One composition per vblank: the next starts COMP_DELAY_MS after this frame is on the screen, with whatever every client committed
            // meanwhile. Composing as soon as one commit arrives would give each vblank to one client.
            unsafe { cr_wait_flip() };
            let t_flip = Instant::now();
            let mut st = CrStats::default();
            unsafe { cr_get_stats(&mut st) };
            let us = |a: Instant, b: Instant| b.saturating_duration_since(a).as_micros() as u64;
            let sample = [
                last_flip.map_or(0, |f| us(f, t_compose)),
                us(t_compose, t_waited),
                us(t_waited, t_built),
                st.acquire_us as u64,
                st.render_us as u64,
                st.present_us as u64,
                us(t_presented, t_flip),
                last_flip.map_or(0, |f| us(f, t_flip)),
            ];
            pace.add(sample);
            pace_all.add(sample);
            last_flip = Some(t_flip);
            if !clients.streams.is_empty() {
                let w = with_clients.get_or_insert((t_flip, t_flip, 0));
                w.1 = t_flip;
                w.2 += 1;
            }
            compose_not_before = t_flip + compose_delay;
        }
        if last_stat.elapsed() >= Duration::from_secs(5) {
            last_stat = Instant::now();
            let mut st = CrStats::default();
            unsafe { cr_get_stats(&mut st) };
            println!("COMP {} frames, {} draws in the last, {} imports, {} uploads, {} clients seen", frames, st.draws, st.imports, st.uploads, clients.seen);
            if kbd >= 0 {
                println!("COMP input: {} key events, {} pointer motions, {} button events; mouse: {} poll wakes, {} REL records (sum {},{}), pointer at {:?}", n_keys, n_moves, n_buttons, n_mouse_wakes, n_rel, sum_dx, sum_dy, comp.pointer());
            }
            if pace.n > 0 {
                println!("COMP pace (5 s): {}", pace.line());
            }
            pace = Pace::default();
        }
    }
    if pace_all.n > 0 {
        println!("COMP pace (all): {}", pace_all.line());
    }
    let mut st = CrStats::default();
    unsafe { cr_get_stats(&mut st) };
    println!(
        "COMP quit after {} frames (up to {} draws), {} imports, {} drops, {} uploads ({} KiB), {} clients seen, {} ms; with clients: {} frames in {} ms",
        frames,
        st.draws_max,
        st.imports,
        st.drops,
        st.uploads,
        st.upload_kb,
        clients.seen,
        t_start.elapsed().as_millis(),
        with_clients.map_or(0, |w| w.2.saturating_sub(1)),
        with_clients.map_or(0, |w| w.1.duration_since(w.0).as_millis())
    );

    if kbd >= 0 {
        println!("COMP input (all): {} key events, {} pointer motions, {} button events; mouse: {} poll wakes, {} REL records (sum {},{}), pointer at {:?}", n_keys, n_moves, n_buttons, n_mouse_wakes, n_rel, sum_dx, sum_dy, comp.pointer());
    }
    let ids: Vec<ClientId> = clients.streams.keys().copied().collect();
    for c in ids {
        clients.drop_client(&mut comp, c);
    }
    drop(listener);
    let _ = std::fs::remove_file(&socket);
    let told = terminate_children();
    if told > 0 {
        println!("COMP ended {} program(s) still running", told);
    }
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
// not under `cargo test`, whose harness has its own `main` (the tests need no renderer: `cargo test` runs the pure parts)
#[cfg_attr(not(test), no_mangle)]
pub extern "C" fn main(argc: c_int, argv: *const *const c_char) -> c_int {
    let args: Vec<String> = (0..argc.max(0) as usize).map(|i| unsafe { CStr::from_ptr(*argv.add(i)) }.to_string_lossy().into_owned()).collect();
    run(&args)
}
