#![no_std]
#![no_main]

//! The windowed terminal (phase 3.5 of `docs/gui/gui-plan.md`): a client
//! of the compositor with `busybox ash` on a pty behind it.
//!
//! `term [cols rows]` (default 80x25, shrunk to fit the screen), usually
//! started as `compositor term`. It opens `/dev/ptmx`; the child makes a
//! session of its own, opens the slave (which so becomes its controlling
//! terminal), puts it on 0/1/2 and execs ash. The parent waits on the
//! compositor's socket and the master under one `epoll`:
//!
//! - bytes from the master → `vt::Parser` → `vt::render` of the damaged
//!   rows → `attach` + `damage` + `commit`, at most one per `frame`
//!   callback (the rows keep collecting damage in the meantime);
//! - `key` from the compositor → `vt::Keyboard` → a write to the master,
//!   repeated while held (the protocol has no key repeat of its own);
//! - the master reads `EIO` or end of file (every slave closed: ash is
//!   gone) or the compositor goes away → the terminal exits, and closing
//!   the master hangs up whatever was still on the slave.
//!
//! The font is the one the kernel console would pick for this screen,
//! which the compositor tells us indirectly: its `configure` suggests
//! half the screen.

extern crate alloc;

use gui::protocol::{Event, Interface, Request, FORMAT_XRGB8888};
use gui::wire::{Decoder, Encoder};
use userspace::args::Args;
use userspace::syscall::{self, AF_UNIX, MAP_SHARED, PROT_READ, PROT_WRITE, SOCK_STREAM};
use userspace::{entry, println};
use vt::{render, Font, Keyboard, Terminal};

entry!(main);

const POOL: u32 = 2;
const BUFFER: u32 = 3;
const SURFACE: u32 = 4;
const FIRST_CALLBACK: u32 = 16;

/// The compositor's title bar and the cascade offset of a first window
/// (`gui::compositor`), kept free so the window fits on screen.
const TITLE_H: usize = gui::compositor::TITLE_H as usize;
const PLACEMENT: usize = 40;

const O_RDWR: i32 = 2;
const O_NONBLOCK: i32 = 0o4000;
const TIOCSWINSZ: u64 = 0x5414;
const TIOCGPTN: u64 = 0x8004_5430;
const TIOCSPTLCK: u64 = 0x4004_5431;
const EAGAIN: i64 = -11;
const SIG_IGN: u64 = 1;

/// Key repeat, as the console's typematic defaults: 500 ms, then ~30/s.
const REPEAT_DELAY_MS: i64 = 500;
const REPEAT_EVERY_MS: i64 = 33;

/// What ash gets: the shell's own (`shell.rs`), but `TERM` says what this
/// emulator is — xterm-like (deferred wrap, `?1049`, `DECCKM`, 256
/// colours), not the kernel console.
const ENVP: [&[u8]; 3] = [
    b"PATH=/tmp/bin:/bin:/mnt/bin\0",
    b"HISTFILE=/mnt/.ash_history\0",
    b"TERM=xterm-256color\0",
];

#[repr(C)]
struct Winsize {
    rows: u16,
    cols: u16,
    xpixel: u16,
    ypixel: u16,
}

fn parse(b: Option<&[u8]>, default: usize) -> usize {
    let Some(b) = b else { return default };
    let mut n = 0usize;
    for &d in b {
        if !d.is_ascii_digit() {
            return default;
        }
        n = n.saturating_mul(10).saturating_add((d - b'0') as usize);
    }
    if n > 0 { n.min(500) } else { default }
}

fn send(fd: i32, reqs: &[Request]) -> bool {
    let mut e = Encoder::new();
    for r in reqs {
        r.encode(&mut e);
    }
    let (bytes, fds) = e.take();
    syscall::send_fds(fd, &bytes, &fds, 0) == bytes.len() as i64
}

/// Writes all of `bytes` to the master, dropping what does not fit: a
/// full input queue means ash is not reading, and typing ahead of it by
/// more than 4 KiB is not worth blocking the window for.
fn write_master(fd: i32, bytes: &[u8]) {
    let mut done = 0;
    while done < bytes.len() {
        let n = syscall::write(fd, &bytes[done..]);
        if n <= 0 {
            return;
        }
        done += n as usize;
    }
}

fn connect() -> Option<i32> {
    let fd = syscall::socket(AF_UNIX as i32, SOCK_STREAM, 0) as i32;
    let (addr, alen) = syscall::SockAddrUn::path(b"/tmp/gui-0");
    for _ in 0..40 {
        if syscall::connect(fd, &addr, alen) == 0 {
            return Some(fd);
        }
        syscall::sleep_ms(50);
    }
    None
}

/// Reads events until the surface's `configure`: the compositor's
/// suggested size, half the screen.
fn wait_configure(fd: i32, dec: &mut Decoder) -> Option<(usize, usize)> {
    let mut buf = [0u8; 512];
    loop {
        let n = syscall::recv(fd, &mut buf);
        if n <= 0 {
            return None;
        }
        dec.push_bytes(&buf[..n as usize]);
        while let Ok(Some(msg)) = dec.next_message() {
            if msg.object != SURFACE {
                continue;
            }
            if let Ok(Event::Configure { width, height, .. }) = Event::decode(Interface::Surface, &msg) {
                return Some((width as usize, height as usize));
            }
        }
    }
}

/// `/dev/ptmx` → `(master, "/dev/pts/N\0")`, unlocked, with `ws` set.
fn open_pty(ws: &Winsize) -> Option<(i32, [u8; 16])> {
    let m = syscall::open(b"/dev/ptmx\0", O_RDWR | O_NONBLOCK) as i32;
    if m < 0 {
        return None;
    }
    let unlock: i32 = 0;
    let mut n: u32 = 0;
    if syscall::ioctl(m, TIOCSPTLCK, &unlock as *const i32 as u64) < 0
        || syscall::ioctl(m, TIOCGPTN, &mut n as *mut u32 as u64) < 0
        || syscall::ioctl(m, TIOCSWINSZ, ws as *const Winsize as u64) < 0
    {
        syscall::close(m);
        return None;
    }
    let mut path = [0u8; 16];
    let prefix = b"/dev/pts/";
    path[..prefix.len()].copy_from_slice(prefix);
    let mut i = prefix.len();
    if n >= 10 {
        path[i] = b'0' + (n / 10) as u8;
        i += 1;
    }
    path[i] = b'0' + (n % 10) as u8;
    Some((m, path))
}

/// The child: session leader on the slave, then ash. Never returns.
fn run_shell(master: i32, slave_path: &[u8]) -> ! {
    syscall::close(master);
    syscall::setsid();
    // Opening the slave without O_NOCTTY as a session leader with no
    // terminal makes it this session's controlling terminal.
    let s = syscall::open(slave_path, O_RDWR) as i32;
    if s < 0 {
        syscall::exit(126);
    }
    for fd in 0..3 {
        syscall::dup2(s, fd);
    }
    // Nothing else goes to the shell: the kernel does not act on
    // close-on-exec, and ash holding our socket would keep the window's
    // connection alive after we die.
    for fd in 3..16 {
        syscall::close(fd);
    }
    let argv: [&[u8]; 2] = [b"busybox\0", b"ash\0"];
    syscall::exec_argv(b"/bin/busybox\0", &argv, &ENVP);
    syscall::exit(127);
}

fn main(args: Args) -> i32 {
    let want_cols = parse(args.get(1), 80);
    let want_rows = parse(args.get(2), 25);

    let Some(sock) = connect() else {
        println!("term: no compositor at /tmp/gui-0");
        return 1;
    };
    // A send to a compositor that just died is EPIPE, and the end of the
    // terminal either way; SIGPIPE would only make it less tidy.
    syscall::sigaction(syscall::SIGPIPE, SIG_IGN);

    let mut dec = Decoder::new();
    if !send(sock, &[Request::CreateSurface { id: SURFACE }]) {
        return 1;
    }
    let Some((half_w, half_h)) = wait_configure(sock, &mut dec) else {
        println!("term: compositor gone before configure");
        return 1;
    };
    let (screen_w, screen_h) = (half_w * 2, half_h * 2);
    let font = Font::for_screen_height(screen_h);
    let (cw, ch) = font.cell();
    let cols = want_cols.min(screen_w.saturating_sub(PLACEMENT) / cw).max(2);
    let rows = want_rows.min(screen_h.saturating_sub(PLACEMENT + TITLE_H) / ch).max(2);
    let (w, h) = (cols * cw, rows * ch);

    let size = (w * h * 4) as u64;
    let mfd = syscall::memfd_create(b"term\0", 0) as i32;
    let base = if mfd >= 0 && syscall::ftruncate(mfd, size) == 0 {
        syscall::mmap(0, size, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0)
    } else {
        -1
    };
    if base <= 0 {
        println!("term: cannot create the pool");
        return 1;
    }
    let px = unsafe { core::slice::from_raw_parts_mut(base as *mut u32, w * h) };

    let ws = Winsize { rows: rows as u16, cols: cols as u16, xpixel: w as u16, ypixel: h as u16 };
    let Some((master, slave_path)) = open_pty(&ws) else {
        println!("term: cannot open a pty");
        return 1;
    };
    let pid = syscall::fork();
    if pid == 0 {
        let end = slave_path.iter().position(|&b| b == 0).unwrap_or(slave_path.len());
        run_shell(master, &slave_path[..=end]);
    }
    if pid < 0 {
        println!("term: fork failed ({})", pid);
        return 1;
    }

    let mut term = Terminal::new(cols, rows);
    let mut kbd = Keyboard::new();
    let mut focused = true; // a new window gets the focus
    let mut cb = FIRST_CALLBACK;

    let damage = term.grid.take_damage();
    render(&term.grid, &damage, &font, focused, px, w);
    let (wi, hi) = (w as i32, h as i32);
    let ok = send(sock, &[
        Request::CreatePool { id: POOL, fd: mfd, size: size as u32 },
        Request::CreateBuffer { pool: POOL, id: BUFFER, offset: 0, width: wi, height: hi, stride: wi * 4, format: FORMAT_XRGB8888 },
        Request::SetTitle { surface: SURFACE, title: "term".into() },
        Request::Attach { surface: SURFACE, buffer: BUFFER },
        Request::Damage { surface: SURFACE, x: 0, y: 0, w: wi, h: hi },
        Request::Frame { surface: SURFACE, id: cb },
        Request::Commit { surface: SURFACE },
    ]);
    syscall::close(mfd);
    if !ok {
        return 1;
    }
    println!("term: {}x{} cells of {}x{} on /dev/pts, ash is pid {}", cols, rows, cw, ch, pid);

    let ep = syscall::epoll_create() as i32;
    syscall::epoll_ctl(ep, syscall::EPOLL_CTL_ADD, sock, syscall::EPOLLIN, 0);
    syscall::epoll_ctl(ep, syscall::EPOLL_CTL_ADD, master, syscall::EPOLLIN, 1);

    // The buffer is ours to draw into again after `release`, and a new
    // frame may go out after `done`.
    let mut released = false;
    let mut frame_due = false;
    let mut dirty = false;
    // A held key: its code and when it next repeats.
    let mut held: Option<(u32, i64)> = None;
    let mut events = [syscall::EpollEvent::default(); 4];
    let mut buf = [0u8; 4096];

    loop {
        let timeout = match held {
            Some((_, at)) => (at - syscall::uptime_ms()).clamp(0, REPEAT_DELAY_MS) as i32,
            None => -1,
        };
        let n = syscall::epoll_wait(ep, &mut events, timeout);
        if n < 0 && n != -4 {
            println!("term: epoll_wait failed ({})", n);
            return 1;
        }
        for ev in &events[..n.max(0) as usize] {
            let tag = ev.data;
            if tag == 1 {
                // The master: everything there is, then draw once.
                let mut total = 0usize;
                loop {
                    let r = syscall::read(master, &mut buf);
                    if r == EAGAIN {
                        break;
                    }
                    if r <= 0 {
                        println!("term: shell gone ({}), bye", r);
                        return 0;
                    }
                    term.feed(&buf[..r as usize]);
                    total += r as usize;
                    // Keys stay responsive under a flood (`yes`).
                    if total >= 256 * 1024 {
                        break;
                    }
                }
                let replies = term.parser.take_replies();
                if !replies.is_empty() {
                    write_master(master, &replies);
                }
                dirty = true;
                continue;
            }
            let r = syscall::recv(sock, &mut buf);
            if r <= 0 {
                println!("term: compositor gone, bye");
                return 0;
            }
            dec.push_bytes(&buf[..r as usize]);
            while let Ok(Some(msg)) = dec.next_message() {
                let iface = match msg.object {
                    1 => Interface::Compositor,
                    BUFFER => Interface::Buffer,
                    SURFACE => Interface::Surface,
                    _ => Interface::Callback,
                };
                let Ok(ev) = Event::decode(iface, &msg) else { continue };
                match ev {
                    Event::Release { .. } => released = true,
                    Event::Done { callback, .. } if callback == cb => frame_due = true,
                    Event::Key { code, pressed, .. } => {
                        let bytes = kbd.key(code, pressed, term.grid.app_cursor());
                        if !bytes.is_empty() {
                            write_master(master, bytes.as_bytes());
                            held = Some((code, syscall::uptime_ms() + REPEAT_DELAY_MS));
                        } else if held.is_some_and(|(c, _)| c == code) && !pressed {
                            held = None;
                        }
                    }
                    Event::Focus { focused: f, .. } => {
                        focused = f;
                        if !f {
                            kbd.release_all();
                            held = None;
                        }
                        dirty = true; // the cursor shows only when focused
                    }
                    Event::Error { object, code, message } => {
                        println!("term: error {} on {}: {}", code, object, message);
                        return 1;
                    }
                    _ => {}
                }
            }
        }

        if let Some((code, at)) = held {
            if syscall::uptime_ms() >= at {
                let bytes = kbd.key(code, true, term.grid.app_cursor());
                write_master(master, bytes.as_bytes());
                held = Some((code, at + REPEAT_EVERY_MS));
            }
        }

        if dirty && released && frame_due {
            let damage = term.grid.take_damage();
            if let Some(r) = render(&term.grid, &damage, &font, focused, px, w) {
                cb += 1;
                let ok = send(sock, &[
                    Request::Attach { surface: SURFACE, buffer: BUFFER },
                    Request::Damage { surface: SURFACE, x: r.x as i32, y: r.y as i32, w: r.w as i32, h: r.h as i32 },
                    Request::Frame { surface: SURFACE, id: cb },
                    Request::Commit { surface: SURFACE },
                ]);
                if !ok {
                    println!("term: compositor gone, bye");
                    return 0;
                }
                released = false;
                frame_due = false;
            }
            dirty = false;
        }
    }
}
