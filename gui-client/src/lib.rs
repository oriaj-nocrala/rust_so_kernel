//! A window under the compositor, for programs written with Rust std.
//!
//! [`Window::open`] connects to the compositor named by `$GUI_DISPLAY` (the compositor sets it for what it starts, `term` for its shell),
//! creates a surface with a title and a shared-memory buffer (a memfd passed with `SCM_RIGHTS`), and from then on the program draws
//! `0x00RRGGBB` pixels into [`Window::frame`], shows them with [`Window::present`] and reads [`Event`]s. Pixels are the screen's: no scaling.
//!
//! The protocol is `gui::protocol` over `gui::wire`, the same code the compositor and `userspace::gfx` (the `no_std` twin of this crate) use.
//! Errors say what failed and why (`docs/ux/principles.md` P1.1): a program that cannot open its window can print the error and exit.

pub mod sys;

use std::collections::VecDeque;
use std::ffi::CStr;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use gui::protocol::{Event as Wire, Interface, Request, COMPOSITOR_ID, FORMAT_XRGB8888};
use gui::region::Rect;
pub use gui::semantic::{self, Node, Role};
use gui::wire::{Decoder, Encoder};

const POOL: u32 = 2;
const BUFFER: u32 = 3;
const SURFACE: u32 = 4;

/// What the compositor tells the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// `code` is a Linux `KEY_*`.
    Key { code: u32, pressed: bool },
    /// The pointer, in the window's pixels.
    Motion { x: i32, y: i32 },
    /// `code` is a Linux `BTN_*` (`BTN_LEFT` is 0x110).
    Button { code: u32, pressed: bool },
    /// Pointer motion while locked ([`Window::lock_pointer`]); `dy` positive is down.
    RelativeMotion { dx: i32, dy: i32 },
    /// The wheel over the window: notches, positive away from the user (scroll up), evdev's `REL_WHEEL` sign.
    Wheel { steps: i32 },
    Focus(bool),
    /// The user resized the window (only after [`Window::set_resizable`]): call [`Window::resize`] with this size (or another) and present.
    Resize { width: usize, height: usize },
    /// The user pressed the close button; the program decides.
    Close,
}

pub struct Window {
    sock: UnixStream,
    dec: Decoder,
    queue: VecDeque<Event>,
    width: usize,
    height: usize,
    pool: sys::Mapping,
    /// Committed and not released yet: the compositor may still be reading it.
    busy: bool,
    suggested: (usize, usize),
}

impl Window {
    /// A window titled `title` with a content of `size` pixels, or the size the compositor suggests (half the screen) if `None`, on the
    /// compositor `$GUI_DISPLAY` names.
    pub fn open(title: &str, size: Option<(usize, usize)>) -> io::Result<Window> {
        let path = std::env::var_os("GUI_DISPLAY").filter(|p| !p.is_empty()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "GUI_DISPLAY is not set: start this program from the compositor (or from term in it)")
        })?;
        let sock = UnixStream::connect(&path).map_err(|e| {
            io::Error::new(e.kind(), format!("cannot connect to the compositor at {}: {}", path.to_string_lossy(), e))
        })?;
        Window::with_stream(sock, title, size)
    }

    /// [`Window::open`] on an already connected socket.
    pub fn with_stream(sock: UnixStream, title: &str, size: Option<(usize, usize)>) -> io::Result<Window> {
        let mut early = Vec::new();
        let mut dec = Decoder::new();
        send(&sock, &[Request::CreateSurface { id: SURFACE }])?;
        // The compositor answers create_surface with a configure: the size it suggests.
        let suggested = loop {
            let ev = read_one(&sock, &mut dec)?;
            match ev {
                Wire::Configure { width, height, .. } => break (width.max(1) as usize, height.max(1) as usize),
                Wire::Error { code, message, .. } => return Err(compositor_error(code, &message)),
                other => early.push(other),
            }
        };
        let (width, height) = size.unwrap_or(suggested);
        send(&sock, &[Request::SetTitle { surface: SURFACE, title: title.into() }])?;
        let pool = new_pool(&sock, width, height, false)?;
        let mut win = Window { sock, dec, queue: VecDeque::new(), width, height, pool, busy: false, suggested };
        for ev in early {
            win.handle(ev)?;
        }
        Ok(win)
    }

    /// The content's size in pixels.
    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// What the compositor suggested at the start (half the screen).
    pub fn suggested_size(&self) -> (usize, usize) {
        self.suggested
    }

    /// Lets the user resize the window, down to `min_w x min_h`; [`Event::Resize`] follows.
    pub fn set_resizable(&mut self, min_w: usize, min_h: usize) -> io::Result<()> {
        send(&self.sock, &[Request::SetResizable { surface: SURFACE, min_w: min_w as i32, min_h: min_h as i32 }])
    }

    /// Locks the pointer to the window (motion then comes as [`Event::RelativeMotion`]), or gives it back.
    pub fn lock_pointer(&mut self, on: bool) -> io::Result<()> {
        send(&self.sock, &[Request::LockPointer { surface: SURFACE, on }])
    }

    /// A new content size. The next [`Window::frame`] is `width x height` and blank. Called for an [`Event::Resize`]: the new buffer is the
    /// answer the compositor waits for.
    pub fn resize(&mut self, width: usize, height: usize) -> io::Result<()> {
        let (width, height) = (width.max(1), height.max(1));
        // The compositor copied the last committed frame, so the old buffer may go at once.
        self.pool = new_pool(&self.sock, width, height, true)?;
        self.width = width;
        self.height = height;
        self.busy = false;
        Ok(())
    }

    /// The pixels to draw (`width * height`, row after row, `0x00RRGGBB`). Waits until the compositor has taken the last presented frame,
    /// which paces the program to the screen; events that arrive meanwhile are kept for [`Window::next_event`].
    pub fn frame(&mut self) -> io::Result<&mut [u32]> {
        while self.busy {
            self.pump(None)?;
        }
        let n = self.width * self.height;
        Ok(&mut self.pool.pixels()[..n])
    }

    /// The window's semantic tree (`gui::semantic`, parents first), shown with the next [`Window::present`]: what tests and agents read.
    pub fn set_semantics(&mut self, nodes: &[Node]) -> io::Result<()> {
        if nodes.len() > semantic::MAX_NODES {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} semantic nodes, at most {}", nodes.len(), semantic::MAX_NODES)));
        }
        // in batches, each well under the wire's 64 KiB message size
        for chunk in nodes.chunks(64) {
            let reqs: Vec<Request> = chunk.iter().map(|n| Request::SemanticsNode { surface: SURFACE, node: n.clone() }).collect();
            send(&self.sock, &reqs)?;
        }
        Ok(())
    }

    /// Shows the frame.
    pub fn present(&mut self) -> io::Result<()> {
        let (w, h) = (self.width as i32, self.height as i32);
        self.present_rect(0, 0, w, h)
    }

    /// Shows the frame, telling the compositor only `w x h` at (`x`, `y`) changed.
    pub fn present_rect(&mut self, x: i32, y: i32, w: i32, h: i32) -> io::Result<()> {
        send(
            &self.sock,
            &[
                Request::Attach { surface: SURFACE, buffer: BUFFER },
                Request::Damage { surface: SURFACE, x, y, w, h },
                Request::Commit { surface: SURFACE },
            ],
        )?;
        self.busy = true;
        Ok(())
    }

    /// The next event: waits up to `timeout` (`None`: until there is one); `Ok(None)` if it passed. The compositor going away is an error
    /// of kind `UnexpectedEof`.
    pub fn next_event(&mut self, timeout: Option<Duration>) -> io::Result<Option<Event>> {
        if self.queue.is_empty() {
            self.pump(timeout)?;
        }
        Ok(self.queue.pop_front())
    }

    /// Reads one batch from the socket (waiting up to `timeout`) and turns it into state and events.
    fn pump(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        let ms = timeout.map_or(-1, |t| t.as_millis().min(i32::MAX as u128) as i32);
        if !sys::wait_readable(self.sock.as_raw_fd(), ms)? {
            return Ok(());
        }
        let mut buf = [0u8; 4096];
        let n = (&self.sock).read(&mut buf)?;
        if n == 0 {
            return Err(gone());
        }
        self.dec.push_bytes(&buf[..n]);
        while let Some(ev) = decode_next(&mut self.dec)? {
            self.handle(ev)?;
        }
        Ok(())
    }

    fn handle(&mut self, ev: Wire) -> io::Result<()> {
        let e = match ev {
            Wire::Release { .. } => {
                self.busy = false;
                return Ok(());
            }
            Wire::Error { code, message, .. } => return Err(compositor_error(code, &message)),
            Wire::Configure { width, height, .. } => {
                self.suggested = (width.max(1) as usize, height.max(1) as usize);
                return Ok(());
            }
            Wire::Key { code, pressed, .. } => Event::Key { code, pressed },
            Wire::Motion { x, y, .. } => Event::Motion { x, y },
            Wire::Button { code, pressed, .. } => Event::Button { code, pressed },
            Wire::RelativeMotion { dx, dy, .. } => Event::RelativeMotion { dx, dy },
            Wire::Axis { steps, .. } => Event::Wheel { steps },
            Wire::Focus { focused, .. } => Event::Focus(focused),
            Wire::Resize { width, height, .. } => Event::Resize { width: width.max(1) as usize, height: height.max(1) as usize },
            Wire::Close { .. } => Event::Close,
            _ => return Ok(()),
        };
        if self.queue.len() < 1024 {
            self.queue.push_back(e);
        }
        Ok(())
    }
}

impl AsRawFd for Window {
    /// The socket, readable when events are waiting: for a program's own `poll`.
    fn as_raw_fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }
}

fn send(sock: &UnixStream, reqs: &[Request]) -> io::Result<()> {
    let mut e = Encoder::new();
    for r in reqs {
        r.encode(&mut e);
    }
    let (bytes, fds) = e.take();
    sys::send_with_fds(sock.as_raw_fd(), &bytes, &fds).map_err(|e| {
        if e.kind() == io::ErrorKind::BrokenPipe {
            gone()
        } else {
            e
        }
    })
}

/// A memfd of `w x h` pixels mapped here and given to the compositor as pool + buffer (replacing the old ones if `replace`). A mapped memfd
/// cannot shrink in this kernel, so a new size is a new memfd.
fn new_pool(sock: &UnixStream, w: usize, h: usize, replace: bool) -> io::Result<sys::Mapping> {
    let size = w.checked_mul(h).and_then(|n| n.checked_mul(4)).filter(|&s| s <= u32::MAX as usize).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("a {}x{} window does not fit in a 32-bit pool", w, h))
    })?;
    let fd = unsafe { OwnedFd::from_raw_fd(sys::memfd(CStr::from_bytes_with_nul(b"gui-client\0").unwrap())?) };
    let file = std::fs::File::from(fd);
    file.set_len(size as u64)?;
    let map = sys::Mapping::new(file.as_raw_fd(), size)
        .map_err(|e| io::Error::new(e.kind(), format!("cannot map the {}x{} window buffer: {}", w, h, e)))?;
    let mut reqs = Vec::new();
    if replace {
        reqs.push(Request::DestroyBuffer { buffer: BUFFER });
        reqs.push(Request::DestroyPool { pool: POOL });
    }
    reqs.push(Request::CreatePool { id: POOL, fd: file.as_raw_fd(), size: size as u32 });
    reqs.push(Request::CreateBuffer {
        pool: POOL,
        id: BUFFER,
        offset: 0,
        width: w as i32,
        height: h as i32,
        stride: (w * 4) as i32,
        format: FORMAT_XRGB8888,
    });
    send(sock, &reqs)?;
    Ok(map) // `file` closes here: the compositor has its own descriptor now
}

/// The next whole event in `dec`, decoded by the interface its object has in this client.
fn decode_next(dec: &mut Decoder) -> io::Result<Option<Wire>> {
    loop {
        let msg = match dec.next_message() {
            Ok(Some(m)) => m,
            Ok(None) => return Ok(None),
            Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad message from the compositor: {:?}", e))),
        };
        let iface = match msg.object {
            COMPOSITOR_ID => Interface::Compositor,
            BUFFER => Interface::Buffer,
            SURFACE => Interface::Surface,
            _ => Interface::Callback,
        };
        // An event this client does not know (a newer compositor) is skipped, as the protocol asks.
        if let Ok(ev) = Wire::decode(iface, &msg) {
            return Ok(Some(ev));
        }
    }
}

/// Blocks for one event (used before the window exists).
fn read_one(sock: &UnixStream, dec: &mut Decoder) -> io::Result<Wire> {
    loop {
        if let Some(ev) = decode_next(dec)? {
            return Ok(ev);
        }
        let mut buf = [0u8; 1024];
        let n = (&*sock).read(&mut buf)?;
        if n == 0 {
            return Err(gone());
        }
        dec.push_bytes(&buf[..n]);
    }
}

/// One window's semantic tree as the compositor has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowTree {
    pub toplevel: u32,
    pub title: String,
    /// The content on the screen; node bounds are relative to its top left.
    pub content: Rect,
    pub focused: bool,
    pub nodes: Vec<Node>,
}

/// Every window's semantic tree, from the compositor `$GUI_DISPLAY` names (bottom window first).
pub fn semantics() -> io::Result<Vec<WindowTree>> {
    let path = std::env::var_os("GUI_DISPLAY")
        .filter(|p| !p.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "GUI_DISPLAY is not set: run this inside the compositor (from term)"))?;
    let sock = UnixStream::connect(&path).map_err(|e| {
        io::Error::new(e.kind(), format!("cannot connect to the compositor at {}: {}", path.to_string_lossy(), e))
    })?;
    semantics_on(&sock)
}

/// [`semantics`] on an already connected socket.
pub fn semantics_on(sock: &UnixStream) -> io::Result<Vec<WindowTree>> {
    const CB: u32 = 2;
    send(sock, &[Request::GetSemantics { id: CB }])?;
    let mut dec = Decoder::new();
    let mut out: Vec<WindowTree> = Vec::new();
    loop {
        match read_one(sock, &mut dec)? {
            Wire::SemanticsWindow { callback: CB, toplevel, title, x, y, w, h, focused } => {
                out.push(WindowTree { toplevel, title, content: Rect::new(x, y, w, h), focused, nodes: Vec::new() })
            }
            Wire::SemanticsNode { callback: CB, node } => match out.last_mut() {
                Some(w) => w.nodes.push(node),
                None => return Err(io::Error::new(io::ErrorKind::InvalidData, "a semantic node before any window")),
            },
            Wire::Done { callback: CB, .. } => return Ok(out),
            Wire::Error { code, message, .. } => return Err(compositor_error(code, &message)),
            _ => {}
        }
    }
}

fn gone() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "the compositor closed the connection")
}

fn compositor_error(code: u32, message: &str) -> io::Error {
    io::Error::other(format!("the compositor refused a request (error {}): {}", code, message))
}
