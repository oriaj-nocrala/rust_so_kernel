//! A picture and input for full-screen programs — the Rust twin of
//! `userspace/c/include/constanos_gfx.h`, with the same behaviour.
//!
//! The program draws a small `w x h` frame of `0x00RRGGBB` pixels and reads
//! evdev-shaped events. Under the compositor (`$GUI_DISPLAY` names its
//! socket: the compositor sets it for what it starts, `term` for its shell)
//! that is a window, the frame scaled by an integer factor into a
//! shared-memory buffer. Otherwise it is the console: `FBIO_BLIT` on
//! `/dev/fb` (scaled by the kernel to the whole screen) plus
//! `/dev/input/event0` (grabbed, so keys don't reach the shell afterwards)
//! and `event1`.
//!
//! Either way events are `EV_KEY` with Linux `KEY_*`/`BTN_*` codes (value 1
//! press, 0 release, 2 autorepeat on the console) and `EV_REL` with
//! `REL_X`/`REL_Y` in PS/2's sign convention (Y positive up).

use alloc::collections::VecDeque;

use gui::protocol::{Event, Interface, Request, FORMAT_XRGB8888};
use gui::wire::{Decoder, Encoder};

use crate::syscall::{self, PollFd, AF_UNIX, MAP_SHARED, POLLIN, PROT_READ, PROT_WRITE, SOCK_STREAM};

pub const EV_KEY: u16 = 1;
pub const EV_REL: u16 = 2;
pub const REL_X: u16 = 0;
pub const REL_Y: u16 = 1;

/// Lock the pointer to the window and report its motion (games).
pub const MOUSE: u32 = 1;
/// The program draws at full resolution: `w x h` is its size in logical
/// pixels, [`Gfx::scale`] the factor it multiplies them by, and
/// [`Gfx::present`] takes a `w*scale x h*scale` frame, shown pixel for
/// pixel. Without it the frame is replicated by that factor, which is
/// right for a game's pixel art and blocky for antialiased text.
pub const HIDPI: u32 = 2;

#[derive(Clone, Copy, Debug)]
pub struct GfxEvent {
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

const POOL: u32 = 2;
const BUFFER: u32 = 3;
const SURFACE: u32 = 4;
const FBIO_BLIT: u64 = 0x4642_0001;
const EVIOCGRAB: u64 = 0x4004_4590;
const EBUSY: i64 = -16;
const TIOCGWINSZ: u64 = 0x5413;

#[repr(C)]
struct BlitArgs {
    ptr: u64,
    width: u32,
    height: u32,
}

enum Backend {
    Console { fb: i32, kbd: i32, mouse: i32, blits: u32 },
    Window(Window),
}

struct Window {
    sock: i32,
    scale: usize,
    pool: &'static mut [u32],
    /// Committed and not released yet.
    busy: bool,
    dec: Decoder,
    queue: VecDeque<GfxEvent>,
    /// Key/button codes < 512 held down, released on focus-out.
    held: [u8; 64],
}

pub struct Gfx {
    w: usize,
    h: usize,
    /// `HIDPI`: the factor the program draws at; 1 otherwise.
    scale: usize,
    b: Backend,
}

impl Gfx {
    /// A `w x h` frame: a window if `display` (`$GUI_DISPLAY`) names a
    /// compositor that answers, the console otherwise. `None` with nothing
    /// to draw on.
    pub fn open(display: Option<&[u8]>, title: &str, w: usize, h: usize, flags: u32) -> Option<Gfx> {
        if let Some(d) = display.filter(|d| !d.is_empty()) {
            if let Some(win) = Window::open(d, title, w, h, flags) {
                let scale = if flags & HIDPI != 0 { win.scale } else { 1 };
                return Some(Gfx { w, h, scale, b: Backend::Window(win) });
            }
            crate::eprintln!("gfx: no compositor at {}, using the console", core::str::from_utf8(d).unwrap_or("?"));
        }
        let fb = syscall::with_cstr("/dev/fb", |p| syscall::open(p, syscall::O_WRONLY)) as i32;
        if fb < 0 {
            return None;
        }
        let kbd = syscall::with_cstr("/dev/input/event0", |p| syscall::open(p, syscall::O_RDONLY)) as i32;
        if kbd >= 0 {
            syscall::ioctl(kbd, EVIOCGRAB, 1);
        }
        let mouse = syscall::with_cstr("/dev/input/event1", |p| syscall::open(p, syscall::O_RDONLY)) as i32;
        // Drop the backlog: the ring fills from every key since boot, and
        // the Enter that started us is still in it.
        let mut rec = [0u8; 24];
        for fd in [kbd, mouse] {
            while fd >= 0 && syscall::read(fd, &mut rec) == 24 {}
        }
        // The kernel's blit scales by the largest integer that fits the
        // screen; a HIDPI program draws at that factor itself.
        let scale = if flags & HIDPI != 0 { console_scale(fb, w, h) } else { 1 };
        Some(Gfx { w, h, scale, b: Backend::Console { fb, kbd, mouse, blits: 0 } })
    }

    /// The factor a `HIDPI` program draws at (1 without the flag): its
    /// frame is `w*scale x h*scale`.
    pub fn scale(&self) -> usize {
        self.scale
    }

    pub fn windowed(&self) -> bool {
        matches!(self.b, Backend::Window(_))
    }

    /// Shows a frame. In a window, first waits for the compositor to have
    /// taken the previous one — which paces the caller to the screen.
    pub fn present(&mut self, px: &[u32]) {
        let (w, h) = (self.w * self.scale, self.h * self.scale);
        match &mut self.b {
            Backend::Console { fb, blits, .. } => {
                let args = BlitArgs { ptr: px.as_ptr() as u64, width: w as u32, height: h as u32 };
                let r = syscall::ioctl(*fb, FBIO_BLIT, &args as *const BlitArgs as u64);
                if r == EBUSY && *blits == 0 {
                    crate::eprintln!("gfx: the screen belongs to a compositor, and GUI_DISPLAY is not set");
                    syscall::exit(1);
                }
                *blits += 1;
            }
            Backend::Window(win) => {
                let s = win.scale / self.scale;
                win.present(px, w, h, s)
            }
        }
    }

    /// The next input event, without blocking.
    pub fn next_event(&mut self) -> Option<GfxEvent> {
        match &mut self.b {
            Backend::Console { kbd, mouse, .. } => {
                let mut rec = [0u8; 24];
                for fd in [*kbd, *mouse] {
                    while fd >= 0 && syscall::read(fd, &mut rec) == 24 {
                        let kind = u16::from_ne_bytes([rec[16], rec[17]]);
                        if kind != EV_KEY && kind != EV_REL {
                            continue; // EV_SYN
                        }
                        let code = u16::from_ne_bytes([rec[18], rec[19]]);
                        let value = i32::from_ne_bytes([rec[20], rec[21], rec[22], rec[23]]);
                        return Some(GfxEvent { kind, code, value });
                    }
                }
                None
            }
            Backend::Window(win) => {
                if win.queue.is_empty() {
                    win.pump(false);
                }
                win.queue.pop_front()
            }
        }
    }
}

impl Drop for Gfx {
    fn drop(&mut self) {
        match &self.b {
            Backend::Console { fb, kbd, mouse, .. } => {
                if *kbd >= 0 {
                    syscall::ioctl(*kbd, EVIOCGRAB, 0);
                    syscall::close(*kbd);
                }
                if *mouse >= 0 {
                    syscall::close(*mouse);
                }
                syscall::close(*fb);
            }
            Backend::Window(win) => {
                syscall::close(win.sock);
            }
        }
    }
}

impl Window {
    fn open(path: &[u8], title: &str, w: usize, h: usize, flags: u32) -> Option<Window> {
        let sock = syscall::socket(AF_UNIX as i32, SOCK_STREAM, 0) as i32;
        if sock < 0 {
            return None;
        }
        let (addr, alen) = syscall::SockAddrUn::path(path);
        if syscall::connect(sock, &addr, alen) < 0 {
            syscall::close(sock);
            return None;
        }
        let mut win = Window {
            sock,
            scale: 1,
            pool: &mut [],
            busy: false,
            dec: Decoder::new(),
            queue: VecDeque::new(),
            held: [0; 64],
        };
        if !win.send(&[Request::CreateSurface { id: SURFACE }]) {
            syscall::close(sock);
            return None;
        }
        // The compositor answers with a configure suggesting half the
        // screen: the largest integer scale up to 3 that fits with the
        // title bar and the cascade offset.
        let (half_w, half_h) = loop {
            let mut buf = [0u8; 512];
            let n = syscall::recv(sock, &mut buf);
            if n <= 0 {
                syscall::close(sock);
                return None;
            }
            win.dec.push_bytes(&buf[..n as usize]);
            let mut size = None;
            while let Ok(Some(msg)) = win.dec.next_message() {
                if msg.object == SURFACE {
                    if let Ok(Event::Configure { width, height, .. }) = Event::decode(Interface::Surface, &msg) {
                        size = Some((width as usize, height as usize));
                    }
                }
            }
            if let Some(s) = size {
                break s;
            }
        };
        let (sw, sh) = (half_w * 2, half_h * 2);
        let mut s = 3;
        while s > 1 && (w * s + 40 > sw || h * s + 60 > sh) {
            s -= 1;
        }
        win.scale = s;
        let (pw, ph) = (w * s, h * s);
        let size = (pw * ph * 4) as u64;
        let mfd = syscall::memfd_create(b"gfx\0", 0) as i32;
        let base = if mfd >= 0 && syscall::ftruncate(mfd, size) == 0 {
            syscall::mmap(0, size, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0)
        } else {
            -1
        };
        if base <= 0 {
            syscall::close(sock);
            return None;
        }
        win.pool = unsafe { core::slice::from_raw_parts_mut(base as *mut u32, pw * ph) };
        let mut reqs = alloc::vec![
            Request::CreatePool { id: POOL, fd: mfd, size: size as u32 },
            Request::CreateBuffer {
                pool: POOL,
                id: BUFFER,
                offset: 0,
                width: pw as i32,
                height: ph as i32,
                stride: (pw * 4) as i32,
                format: FORMAT_XRGB8888,
            },
            Request::SetTitle { surface: SURFACE, title: title.into() },
        ];
        if flags & MOUSE != 0 {
            reqs.push(Request::LockPointer { surface: SURFACE, on: true });
        }
        let ok = win.send(&reqs);
        syscall::close(mfd); // the compositor has its own now
        if !ok {
            syscall::close(sock);
            return None;
        }
        Some(win)
    }

    fn send(&self, reqs: &[Request]) -> bool {
        let mut e = Encoder::new();
        for r in reqs {
            r.encode(&mut e);
        }
        let (bytes, fds) = e.take();
        syscall::send_fds(self.sock, &bytes, &fds, 0) == bytes.len() as i64
    }

    fn push(&mut self, kind: u16, code: u16, value: i32) {
        if self.queue.len() < 256 {
            self.queue.push_back(GfxEvent { kind, code, value });
        }
    }

    fn note_key(&mut self, code: u32, pressed: bool) {
        if code < 512 {
            let (i, bit) = ((code / 8) as usize, 1u8 << (code % 8));
            if pressed {
                self.held[i] |= bit;
            } else {
                self.held[i] &= !bit;
            }
        }
        self.push(EV_KEY, code as u16, pressed as i32);
    }

    /// Reads what the socket has (blocking for more if `wait`) and turns it
    /// into state and events. The compositor gone ends the program, as its
    /// window was all it had.
    fn pump(&mut self, wait: bool) {
        if !wait {
            let mut pfd = [PollFd { fd: self.sock, events: POLLIN, revents: 0 }];
            if syscall::poll(&mut pfd, 0) <= 0 {
                return;
            }
        }
        let mut buf = [0u8; 2048];
        let n = syscall::recv(self.sock, &mut buf);
        if n <= 0 {
            syscall::exit(0);
        }
        self.dec.push_bytes(&buf[..n as usize]);
        while let Ok(Some(msg)) = self.dec.next_message() {
            let iface = match msg.object {
                1 => Interface::Compositor,
                BUFFER => Interface::Buffer,
                SURFACE => Interface::Surface,
                _ => Interface::Callback,
            };
            let Ok(ev) = Event::decode(iface, &msg) else { continue };
            match ev {
                Event::Release { .. } => self.busy = false,
                Event::Key { code, pressed, .. } | Event::Button { code, pressed, .. } => self.note_key(code, pressed),
                Event::RelativeMotion { dx, dy, .. } => {
                    if dx != 0 {
                        self.push(EV_REL, REL_X, dx);
                    }
                    if dy != 0 {
                        self.push(EV_REL, REL_Y, -dy); // screen → PS/2
                    }
                }
                Event::Focus { focused: false, .. } => {
                    for code in 0..512u32 {
                        if self.held[(code / 8) as usize] & (1 << (code % 8)) != 0 {
                            self.note_key(code, false);
                        }
                    }
                }
                Event::Error { code, message, .. } => {
                    crate::eprintln!("gfx: compositor error {}: {}", code, message);
                    syscall::exit(1);
                }
                _ => {}
            }
        }
    }

    /// Shows a `w x h` frame replicated `s` times each way into the pool.
    fn present(&mut self, px: &[u32], w: usize, h: usize, s: usize) {
        while self.busy {
            self.pump(true);
        }
        let pw = w * s;
        for y in 0..h {
            let row = y * s * pw;
            let src = &px[y * w..(y + 1) * w];
            for (x, &c) in src.iter().enumerate() {
                self.pool[row + x * s..row + x * s + s].fill(c);
            }
            for k in 1..s {
                self.pool.copy_within(row..row + pw, row + k * pw);
            }
        }
        if !self.send(&[
            Request::Attach { surface: SURFACE, buffer: BUFFER },
            Request::Damage { surface: SURFACE, x: 0, y: 0, w: pw as i32, h: (h * s) as i32 },
            Request::Commit { surface: SURFACE },
        ]) {
            syscall::exit(0);
        }
        self.busy = true;
    }
}

/// The factor `FBIO_BLIT` would scale a `w x h` frame by: the largest
/// integer that fits the screen, whose size in pixels `TIOCGWINSZ` gives.
/// 1 if the kernel does not say.
fn console_scale(fb: i32, w: usize, h: usize) -> usize {
    let mut ws = [0u16; 4];
    if syscall::ioctl(fb, TIOCGWINSZ, ws.as_mut_ptr() as u64) != 0 {
        return 1;
    }
    let (sw, sh) = (ws[2] as usize, ws[3] as usize);
    if w == 0 || h == 0 {
        return 1;
    }
    (sw / w).min(sh / h).max(1)
}
