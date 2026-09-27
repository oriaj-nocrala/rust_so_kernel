#![no_std]
#![no_main]

//! The compositor (phase 2.5 of `docs/gui/gui-plan.md`): the machine
//! around `gui::compositor::Compositor`.
//!
//! Holds `/dev/fb0` (graphics mode, the screen's RAM copy mapped) and
//! `/dev/input/event0` (grabbed, so keys stop reaching the console) and
//! `event1`, listens on `/tmp/gui-0`, and waits on all of them with one
//! `epoll`. Every client message goes into the state machine; whatever it
//! queued — events, disconnects, fds to close, damage — is carried out
//! here.
//!
//! Pacing. With `/dev/vblank` (the GPU driver's `gpu=vblank`), the screen's
//! RAM copy is the back buffer: each vblank first flushes what the
//! previous frame composed — the copy starts in the blanking interval and
//! outruns the beam, so no tearing — and then composes the next frame. A
//! vblank that does not come within `VSYNC_GRACE_MS` is not waited for.
//! Without it, at most one compose + `FBIO_FLUSH` every 16 ms. With the
//! kernel's page flipping (`gpu=scanout`) the flush is a flip at the next
//! vblank; `EBUSY` means the previous flip has not landed, and that frame's
//! rectangles go out with the next one.
//!
//! `compositor [prog...]` starts each `prog` (from `/bin` or `/mnt/bin`
//! unless it has a `/`) once the socket is listening; with no arguments it
//! is a session and starts `panel` (phase 4), if there is one. Window
//! titles are drawn here with the text engine (`userspace::text`), which
//! is why this program lives on the disk. **Ctrl+Alt+Backspace quits**: with
//! the keyboard grabbed it is the only way out without a second terminal.
//! Quitting closes `/dev/fb0` and the console comes back.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use alloc::string::String;
use draw::Canvas;
use gui::compositor::{scale_for, ClientId, Compositor, PoolMem, TitleText, TITLE_FOCUSED, TITLE_UNFOCUSED};
use gui::region::Rect;
use gui::wire::Encoder;
use userspace::args::Args;
use userspace::launch;
use userspace::text::{Style, Text, SANS};
use userspace::syscall::{self, EpollEvent, AF_UNIX, MAP_SHARED, PROT_READ, PROT_WRITE, SOCK_NONBLOCK, SOCK_STREAM};
use userspace::{entry, println};

entry!(main);

const SOCKET_PATH: &[u8] = b"/tmp/gui-0";
const FRAME_MS: i64 = 16;
/// Longest wait for a vblank before presenting anyway (three frames at 60 Hz).
const VSYNC_GRACE_MS: i64 = 50;

const FBIO_GET_INFO: u64 = 0x4642_0010;
const FBIO_FLUSH: u64 = 0x4642_0011;
const EBUSY: i64 = -16;
const EVIOCGRAB: u64 = 0x4004_4590;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;

// epoll data tags; clients are TAG_CLIENT + id.
const TAG_LISTEN: u64 = 1;
const TAG_KBD: u64 = 2;
const TAG_MOUSE: u64 = 3;
const TAG_VBLANK: u64 = 4;
const TAG_CLIENT: u64 = 1000;

#[repr(C)]
#[derive(Default)]
struct Fb0Info {
    width: u32,
    height: u32,
    stride: u32,
    bytes_per_pixel: u32,
    offset: u64,
    map_len: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Fb0Rect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

#[repr(C)]
struct Fb0Flush {
    count: u32,
    _pad: u32,
    rects: [Fb0Rect; 16],
}

/// `a`'s rectangles followed by `b`'s: a frame whose flush the kernel
/// refused (`EBUSY`, a page flip still pending) is presented together with
/// the next one. Past 16 rectangles, their bounding box.
fn merge(a: Option<Fb0Flush>, b: Fb0Flush) -> Fb0Flush {
    let Some(mut a) = a else { return b };
    let (na, nb) = (a.count as usize, b.count as usize);
    if na + nb <= a.rects.len() {
        a.rects[na..na + nb].copy_from_slice(&b.rects[..nb]);
        a.count += b.count;
        return a;
    }
    let all = a.rects[..na].iter().chain(&b.rects[..nb]);
    let (x0, y0) = all.clone().fold((u32::MAX, u32::MAX), |(x, y), r| (x.min(r.x), y.min(r.y)));
    let (x1, y1) = all.fold((0, 0), |(x, y), r| (x.max(r.x + r.w), y.max(r.y + r.h)));
    let mut m = Fb0Flush { count: 1, _pad: 0, rects: [Fb0Rect::default(); 16] };
    m.rects[0] = Fb0Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
    m
}

/// Marks the current vblank seen (`/dev/vblank`'s record: sequence number,
/// time in ns), so the next poll waits for the one after it.
fn read_vblank(fd: i32) {
    let mut rec = [0u8; 16];
    syscall::read(fd, &mut rec);
}

/// A client's pool, mapped shared; unmapped when the last buffer of it
/// goes.
struct Mapping {
    addr: u64,
    len: usize,
}

impl PoolMem for Mapping {
    fn as_ptr(&self) -> *const u8 {
        self.addr as *const u8
    }
    fn len(&self) -> usize {
        self.len
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        syscall::munmap(self.addr, self.len as u64);
    }
}

/// Maps a pool the client says is `size` bytes — after checking that the
/// memfd really is that big, since touching a page past its end would
/// fault this process, not the client.
fn map_pool(fd: i32, size: usize) -> Option<Mapping> {
    let st = syscall::fstat(fd).ok()?;
    if size == 0 || (st.st_size as u64) < size as u64 {
        return None;
    }
    let len = (size + 4095) & !4095;
    let a = syscall::mmap(0, len as u64, PROT_READ, MAP_SHARED, fd, 0);
    if a <= 0 {
        return None;
    }
    Some(Mapping { addr: a as u64, len })
}

fn open_path(path: &str, flags: i32) -> i32 {
    syscall::with_cstr(path, |p| syscall::open(p, flags)) as i32
}

/// A window title rasterised once, over its bar's colour: redrawn only
/// when the title, the focus or the bar's width changes.
struct TitleImg {
    title: String,
    focused: bool,
    w: i32,
    h: i32,
    px: Vec<u32>,
}

/// Paints titles for `Compositor::compose_with` with the text engine
/// (proportional Noto from `/mnt`, or the bitmap fallback without it).
struct Titles {
    text: Text,
    cache: BTreeMap<u32, TitleImg>,
    size: f32,
}

impl Titles {
    fn new(scale: i32) -> Titles {
        Titles { text: Text::load(), cache: BTreeMap::new(), size: 13.0 * scale as f32 }
    }

    fn paint(&mut self, t: &TitleText, clip: Rect, dst: &mut [u32], stride: usize) {
        let a = t.area;
        let stale = self.cache.get(&t.id).is_none_or(|c| c.title != t.title || c.focused != t.focused || (c.w, c.h) != (a.w, a.h));
        if stale {
            let bg = if t.focused { TITLE_FOCUSED } else { TITLE_UNFOCUSED };
            let fg = if t.focused { 0x00F0_F0F0 } else { 0x00B0_B0B8 };
            let mut px = alloc::vec![bg; (a.w.max(0) * a.h.max(0)) as usize];
            if a.w > 0 && a.h > 0 {
                let mut cv = Canvas::new(&mut px, a.w as usize, a.h as usize, a.w as usize);
                let st = Style::new(SANS, self.size).bold().color(fg);
                let (_, lh) = self.text.measure("Hg", &st, None);
                self.text.draw(&mut cv, t.title, &st, None, 0, (a.h - lh) / 2);
            }
            self.cache.insert(t.id, TitleImg { title: String::from(t.title), focused: t.focused, w: a.w, h: a.h, px });
        }
        let img = &self.cache[&t.id];
        for y in clip.y..clip.bottom() {
            let src = &img.px[((y - a.y) * a.w + (clip.x - a.x)) as usize..][..clip.w as usize];
            dst[y as usize * stride + clip.x as usize..][..clip.w as usize].copy_from_slice(src);
        }
    }

    /// Forgets the titles of windows that are gone.
    fn prune(&mut self, comp: &Compositor<Mapping>) {
        let live = comp.toplevels();
        self.cache.retain(|id, _| live.iter().any(|(t, _)| t == id));
    }
}

struct Io {
    ep: i32,
    /// client id -> socket fd
    sockets: BTreeMap<ClientId, i32>,
}

impl Io {
    fn drop_client(&mut self, comp: &mut Compositor<Mapping>, c: ClientId) {
        comp.remove_client(c);
        if let Some(fd) = self.sockets.remove(&c) {
            syscall::epoll_ctl(self.ep, syscall::EPOLL_CTL_DEL, fd, 0, 0);
            syscall::close(fd);
            println!("compositor: client {} gone", c);
        }
    }

    /// Carries out what the state machine queued.
    fn flush(&mut self, comp: &mut Compositor<Mapping>) {
        let mut per_client: BTreeMap<ClientId, Encoder> = BTreeMap::new();
        for (c, ev) in comp.take_events() {
            ev.encode(per_client.entry(c).or_default());
        }
        let mut dead = Vec::new();
        for (c, mut e) in per_client {
            let Some(&fd) = self.sockets.get(&c) else { continue };
            let (bytes, _) = e.take();
            // Never block on a client: one that does not read its events
            // is dropped, as libwayland's compositor side does.
            let n = syscall::send_flags(fd, &bytes, syscall::MSG_DONTWAIT);
            if n != bytes.len() as i64 {
                println!("compositor: client {} not reading ({}), dropped", c, n);
                dead.push(c);
            }
        }
        for c in comp.take_disconnects() {
            println!("compositor: client {} disconnected for a protocol error", c);
            dead.push(c);
        }
        for c in dead {
            self.drop_client(comp, c);
        }
        for fd in comp.take_fds_to_close() {
            syscall::close(fd);
        }
    }
}

/// Drains an evdev fd, one 24-byte `struct input_event` per `read`.
fn read_input(fd: i32, mut f: impl FnMut(u16, u16, i32)) {
    let mut rec = [0u8; 24];
    for _ in 0..256 {
        if syscall::read(fd, &mut rec) != 24 {
            return;
        }
        let ty = u16::from_ne_bytes([rec[16], rec[17]]);
        let code = u16::from_ne_bytes([rec[18], rec[19]]);
        let value = i32::from_ne_bytes([rec[20], rec[21], rec[22], rec[23]]);
        f(ty, code, value);
    }
}

fn main(args: Args) -> i32 {
    // The keyboard grab still lets ^C/^\/^Z signal the console's
    // foreground group — ours — as an escape hatch for a hung grabber
    // (`kernel/src/keyboard.rs`). A ^C or ^Z typed into `term` is meant for
    // the shell in that window, so both are ignored here; ^\ (SIGQUIT)
    // still ends the compositor, which is the hatch that stays, and the
    // price is that a ^\ typed into a window does too. Children get the
    // defaults back and a group of their own (`spawn`).
    syscall::sigaction(syscall::SIGINT, 1);
    syscall::sigaction(syscall::SIGTSTP, 1);
    // ── the screen ────────────────────────────────────────────────────────
    let fb = open_path("/dev/fb0", syscall::O_RDWR);
    if fb < 0 {
        println!("compositor: open /dev/fb0 failed ({})", fb);
        return 1;
    }
    let mut info = Fb0Info::default();
    if syscall::ioctl(fb, FBIO_GET_INFO, &mut info as *mut Fb0Info as u64) != 0 || info.bytes_per_pixel != 4 {
        println!("compositor: FBIO_GET_INFO failed");
        return 1;
    }
    let base = syscall::mmap(0, info.map_len, PROT_READ | PROT_WRITE, MAP_SHARED, fb, 0);
    if base <= 0 {
        println!("compositor: mmap /dev/fb0 failed ({})", base);
        return 1;
    }
    let screen: &mut [u32] = unsafe {
        core::slice::from_raw_parts_mut((base as u64 + info.offset) as *mut u32, info.stride as usize * info.height as usize)
    };
    println!("compositor: {}x{} stride {}", info.width, info.height, info.stride);

    // ── input ─────────────────────────────────────────────────────────────
    let kbd = open_path("/dev/input/event0", syscall::O_RDONLY);
    let mouse = open_path("/dev/input/event1", syscall::O_RDONLY);
    if kbd < 0 || mouse < 0 {
        println!("compositor: cannot open input ({} {})", kbd, mouse);
        return 1;
    }
    syscall::ioctl(kbd, EVIOCGRAB, 1);
    // What the rings hold from before (every key since boot) is not ours.
    read_input(kbd, |_, _, _| {});
    read_input(mouse, |_, _, _| {});

    // ── the socket ────────────────────────────────────────────────────────
    let lfd = syscall::socket(AF_UNIX as i32, SOCK_STREAM | SOCK_NONBLOCK, 0) as i32;
    let (addr, alen) = syscall::SockAddrUn::path(SOCKET_PATH);
    syscall::with_cstr("/tmp/gui-0", |p| syscall::unlink(p)); // a dead compositor's
    if lfd < 0 || syscall::bind(lfd, &addr, alen) < 0 || syscall::listen(lfd, 8) < 0 {
        println!("compositor: cannot listen on /tmp/gui-0");
        return 1;
    }

    let ep = syscall::epoll_create() as i32;
    syscall::epoll_ctl(ep, syscall::EPOLL_CTL_ADD, lfd, syscall::EPOLLIN, TAG_LISTEN);
    syscall::epoll_ctl(ep, syscall::EPOLL_CTL_ADD, kbd, syscall::EPOLLIN, TAG_KBD);
    syscall::epoll_ctl(ep, syscall::EPOLL_CTL_ADD, mouse, syscall::EPOLLIN, TAG_MOUSE);

    let mut comp: Compositor<Mapping> = Compositor::new(info.width as i32, info.height as i32);
    let mut io = Io { ep, sockets: BTreeMap::new() };

    let mut titles = Titles::new(scale_for(info.height as i32));
    if titles.text.missing > 0 {
        println!("compositor: {} font files missing, titles in the bitmap font", titles.text.missing);
    }
    let start = |cmd: &[u8]| {
        let pid = launch::spawn(cmd);
        let name = core::str::from_utf8(cmd).unwrap_or("?");
        if pid > 0 {
            println!("compositor: started {} (pid {})", name, pid);
        } else {
            println!("compositor: cannot start {} ({})", name, pid);
        }
    };
    if args.len() > 1 {
        for i in 1..args.len() {
            start(args.get(i).unwrap());
        }
    } else if launch::find(b"panel").is_some() {
        start(b"panel");
    } else {
        println!("compositor: no panel, an empty session (Ctrl+Alt+Backspace quits)");
    }

    // ENODEV without the GPU driver: then the timer paces.
    let vblank = open_path("/dev/vblank", syscall::O_RDONLY);
    println!("compositor: pacing by {}", if vblank >= 0 { "vblank (/dev/vblank)" } else { "a 16 ms timer" });

    let mut last_frame: i64 = -FRAME_MS;
    let mut frames: u64 = 0;
    let mut compose_ms_total: i64 = 0;
    let (mut mdx, mut mdy) = (0i32, 0i32);
    let mut buf = alloc::vec![0u8; 4096];
    let mut evs = [EpollEvent::default(); 16];
    // Vblank pacing: what the last compose left in the RAM copy for the
    // next vblank to flush; whether `vblank` is in the epoll set; since
    // when a vblank is awaited; presents on a vblank and by grace timeout.
    let mut pending: Option<Fb0Flush> = None;
    let mut watching = false;
    let mut wait_from: i64 = 0;
    let (mut on_vblank, mut on_timeout) = (0u64, 0u64);
    // Presents the kernel refused because its page flip was still pending.
    let mut flip_busy = 0u64;

    while !comp.quit_requested() {
        let busy = comp.has_damage() || comp.has_frame_callbacks();
        let want = vblank >= 0 && (busy || pending.is_some());
        if want != watching {
            if want {
                read_vblank(vblank); // a vblank from while idle is not this frame's
                syscall::epoll_ctl(ep, syscall::EPOLL_CTL_ADD, vblank, syscall::EPOLLIN, TAG_VBLANK);
                wait_from = syscall::uptime_ms();
            } else {
                syscall::epoll_ctl(ep, syscall::EPOLL_CTL_DEL, vblank, 0, 0);
            }
            watching = want;
        }
        let now = syscall::uptime_ms();
        let timeout = if want {
            (wait_from + VSYNC_GRACE_MS - now).clamp(0, VSYNC_GRACE_MS) as i32
        } else if busy {
            (last_frame + FRAME_MS - now).clamp(0, FRAME_MS) as i32
        } else {
            -1
        };
        let n = syscall::epoll_wait(ep, &mut evs, timeout);
        let evs = &evs[..n.max(0) as usize];
        // Present first, before any client message: the flush has to start
        // inside the blanking interval to stay ahead of the beam.
        let mut tick = false;
        if want {
            if evs.iter().any(|e| e.data == TAG_VBLANK) {
                read_vblank(vblank);
                on_vblank += 1;
                tick = true;
            } else if syscall::uptime_ms() >= wait_from + VSYNC_GRACE_MS {
                on_timeout += 1;
                tick = true;
            }
            if tick {
                if let Some(fl) = pending.take() {
                    if syscall::ioctl(fb, FBIO_FLUSH, &fl as *const Fb0Flush as u64) == EBUSY {
                        pending = Some(fl); // the last flip has not landed: next vblank
                        flip_busy += 1;
                    }
                }
                wait_from = syscall::uptime_ms();
            }
        }
        comp.set_time(syscall::uptime_ms() as u32);
        while syscall::reap_any() > 0 {} // what we started and has exited
        for ev in evs {
            let tag = ev.data;
            match tag {
                TAG_VBLANK => {} // handled above
                TAG_LISTEN => loop {
                    let fd = syscall::accept4(lfd, SOCK_NONBLOCK);
                    if fd < 0 {
                        break;
                    }
                    let c = comp.add_client();
                    io.sockets.insert(c, fd as i32);
                    syscall::epoll_ctl(ep, syscall::EPOLL_CTL_ADD, fd as i32, syscall::EPOLLIN, TAG_CLIENT + c as u64);
                    println!("compositor: client {} connected", c);
                },
                TAG_KBD => read_input(kbd, |ty, code, value| {
                    if ty == EV_KEY {
                        comp.key(code as u32, value != 0);
                    }
                }),
                TAG_MOUSE => read_input(mouse, |ty, code, value| match (ty, code) {
                    (EV_REL, REL_X) => mdx += value,
                    // PS/2's convention: positive is up. The screen's is down.
                    (EV_REL, REL_Y) => mdy -= value,
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
                t if t >= TAG_CLIENT => {
                    let c = (t - TAG_CLIENT) as ClientId;
                    let Some(&fd) = io.sockets.get(&c) else { continue };
                    loop {
                        let mut fds = [-1i32; syscall::MAX_PASSED_FDS];
                        match syscall::recv_fds(fd, &mut buf, &mut fds, syscall::MSG_DONTWAIT) {
                            Ok(r) if r.len == 0 && r.nfds == 0 => {
                                io.drop_client(&mut comp, c);
                                break;
                            }
                            Ok(r) => comp.client_data(c, &buf[..r.len], &fds[..r.nfds], &mut map_pool),
                            Err(_) => break, // EAGAIN: all read
                        }
                        if !comp.has_client(c) {
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
        io.flush(&mut comp);

        let now = syscall::uptime_ms();
        let due = if vblank >= 0 { tick } else { now >= last_frame + FRAME_MS };
        if (comp.has_damage() || comp.has_frame_callbacks()) && due {
            let t0 = syscall::uptime_ms();
            let rects = comp.compose_with(screen, info.stride as usize, &mut |t, clip, dst, stride| titles.paint(t, clip, dst, stride));
            titles.prune(&comp);
            if !rects.is_empty() {
                let mut fl = Fb0Flush { count: rects.len() as u32, _pad: 0, rects: [Fb0Rect::default(); 16] };
                for (d, r) in fl.rects.iter_mut().zip(&rects) {
                    *d = Fb0Rect { x: r.x as u32, y: r.y as u32, w: r.w as u32, h: r.h as u32 };
                }
                if vblank >= 0 {
                    pending = Some(merge(pending.take(), fl)); // the next vblank shows it
                } else {
                    syscall::ioctl(fb, FBIO_FLUSH, &fl as *const Fb0Flush as u64);
                }
            }
            compose_ms_total += syscall::uptime_ms() - t0;
            frames += 1;
            comp.frame_done(now as u32);
            last_frame = now;
            io.flush(&mut comp);
        }
    }

    println!("compositor: quit after {} frames, {} ms composing + flushing", frames, compose_ms_total);
    if vblank >= 0 {
        println!("compositor: presented {} times on vblank, {} by the {} ms grace timeout, {} deferred (flip pending)", on_vblank, on_timeout, VSYNC_GRACE_MS, flip_busy);
        syscall::close(vblank);
    }
    for (_, fd) in core::mem::take(&mut io.sockets) {
        syscall::close(fd);
    }
    syscall::close(lfd);
    syscall::with_cstr("/tmp/gui-0", |p| syscall::unlink(p));
    syscall::munmap(base as u64, info.map_len);
    syscall::close(fb); // the console comes back
    syscall::close(kbd); // and the keyboard with it
    0
}
