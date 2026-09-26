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
//! here. At most one compose + `FBIO_FLUSH` every 16 ms.
//!
//! `compositor [prog...]` starts each `prog` (from `/bin` unless it has a
//! `/`) once the socket is listening. **Ctrl+Alt+Backspace quits**: with
//! the keyboard grabbed it is the only way out without a second terminal.
//! Quitting closes `/dev/fb0` and the console comes back.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use gui::compositor::{ClientId, Compositor, PoolMem};
use gui::wire::Encoder;
use userspace::args::Args;
use userspace::syscall::{self, EpollEvent, AF_UNIX, MAP_SHARED, PROT_READ, PROT_WRITE, SOCK_NONBLOCK, SOCK_STREAM};
use userspace::{entry, println};

entry!(main);

const SOCKET_PATH: &[u8] = b"/tmp/gui-0";
const FRAME_MS: i64 = 16;

const FBIO_GET_INFO: u64 = 0x4642_0010;
const FBIO_FLUSH: u64 = 0x4642_0011;
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

const GUI_DISPLAY_ENV: &[u8] = b"GUI_DISPLAY=/tmp/gui-0\0";

/// Where a bare program name is looked for, in order — ash's `PATH` minus
/// `/tmp/bin` (BusyBox applets are not graphical): the embedded programs,
/// then the disk's, where everything linking `userspace::text` lives.
const SEARCH: [&[u8]; 2] = [b"/bin/", b"/mnt/bin/"];

/// Starts `name`: as given if it contains a `/`, else the first of
/// `SEARCH` that has it.
fn spawn(name: &[u8]) {
    let mut path = [0u8; 64];
    let mut n = 0;
    let prefixes: &[&[u8]] = if name.contains(&b'/') { &[b""] } else { &SEARCH };
    for prefix in prefixes {
        n = (prefix.len() + name.len()).min(63);
        path[..prefix.len()].copy_from_slice(prefix);
        path[prefix.len()..n].copy_from_slice(&name[..n - prefix.len()]);
        path[n] = 0;
        if syscall::stat(&path[..=n]).is_ok() {
            break;
        }
    }
    let pid = syscall::fork();
    if pid == 0 {
        // A group of its own with the default ^C/^Z: the console's
        // foreground group is ours, and a key typed into a terminal
        // window would otherwise reach the client twice — once through
        // the window and once as a console signal (see `main`).
        syscall::setpgid(0, 0);
        syscall::sigaction(syscall::SIGINT, 0);
        syscall::sigaction(syscall::SIGTSTP, 0);
        // Nothing past stdio goes to the client: the kernel does not act on
        // close-on-exec, and a child holding our /dev/fb0 or grabbed
        // event0 would keep graphics mode and the keyboard after we die.
        for fd in 3..16 {
            syscall::close(fd);
        }
        let p = &path[..=n];
        // How a program finds us (read by constanos_gfx.h and handed on
        // by `term` to its shell), as WAYLAND_DISPLAY is.
        syscall::exec_argv(p, &[p], &[GUI_DISPLAY_ENV]);
        syscall::exit(127);
    }
    println!("compositor: started {} (pid {})", core::str::from_utf8(&path[..n]).unwrap_or("?"), pid);
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

    for i in 1..args.len() {
        spawn(args.get(i).unwrap());
    }

    let mut last_frame: i64 = -FRAME_MS;
    let mut frames: u64 = 0;
    let mut compose_ms_total: i64 = 0;
    let (mut mdx, mut mdy) = (0i32, 0i32);
    let mut buf = alloc::vec![0u8; 4096];
    let mut evs = [EpollEvent::default(); 16];

    while !comp.quit_requested() {
        let busy = comp.has_damage() || comp.has_frame_callbacks();
        let timeout = if busy { (last_frame + FRAME_MS - syscall::uptime_ms()).clamp(0, FRAME_MS) as i32 } else { -1 };
        let n = syscall::epoll_wait(ep, &mut evs, timeout);
        for ev in evs.iter().take(n.max(0) as usize) {
            let tag = ev.data;
            match tag {
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
        if (comp.has_damage() || comp.has_frame_callbacks()) && now >= last_frame + FRAME_MS {
            let t0 = syscall::uptime_ms();
            let rects = comp.compose(screen, info.stride as usize);
            if !rects.is_empty() {
                let mut fl = Fb0Flush { count: rects.len() as u32, _pad: 0, rects: [Fb0Rect::default(); 16] };
                for (d, r) in fl.rects.iter_mut().zip(&rects) {
                    *d = Fb0Rect { x: r.x as u32, y: r.y as u32, w: r.w as u32, h: r.h as u32 };
                }
                syscall::ioctl(fb, FBIO_FLUSH, &fl as *const Fb0Flush as u64);
            }
            compose_ms_total += syscall::uptime_ms() - t0;
            frames += 1;
            comp.frame_done(now as u32);
            last_frame = now;
            io.flush(&mut comp);
        }
    }

    println!("compositor: quit after {} frames, {} ms composing + flushing", frames, compose_ms_total);
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
