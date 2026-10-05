//! The compositor's state: clients, their objects, surfaces, stacking,
//! focus, the pointer, window management, and [`Compositor::compose`].
//!
//! **Driving it.** The program around it (phase 2.5) calls
//! [`Compositor::add_client`] on `accept`, [`Compositor::client_data`] with
//! whatever `recvmsg` returned, the input methods with evdev events, and
//! [`Compositor::compose`] + [`Compositor::frame_done`] once per frame.
//! Then it carries out what accumulated: [`Compositor::take_events`] (send
//! each), [`Compositor::take_disconnects`] (close those sockets),
//! [`Compositor::take_fds_to_close`]. Nothing here makes a syscall; mapping
//! a pool is the one thing that must happen synchronously, so
//! `client_data` takes a closure that does it.
//!
//! **Surfaces keep their own copy.** A `commit` copies the damaged part of
//! the attached buffer into the surface's store and sends `release` right
//! away, so the client may draw its next frame into the same buffer at
//! once. Composing reads only stores, never client memory — which is what
//! lets a window that gets uncovered be repainted at all, and means a
//! client scribbling on its pool mid-compose can tear nothing.
//!
//! **GPU buffers** (`create_gpu_buffer`, layer 4 of `docs/gpu/g5-graphics-stack-plan.md`) are the exception to
//! that rule: a GPU compositor reads the client's buffer itself, so nothing is copied and `release` is *not*
//! sent at `commit`. The buffer stays the surface's content until a later commit replaces it, and then it
//! is released only once the host says the frame that may have read it is done
//! ([`Compositor::gpu_frame_done`]). As with everything here, effects come back as data: [`GpuOp`]
//! (import this descriptor, drop that buffer) and [`Compositor::draw_list`] (what to draw, back to front)
//! instead of [`Compositor::compose`]'s pixels. Windows with ordinary shm pools (the panel, `term`) keep
//! working: they come out of the list as [`DrawOp::Cpu`], their store to be uploaded.
//!
//! **A buffer that does not fit its pool is a protocol error**, checked at
//! `create_buffer` against the pool's size — never a read out of bounds.
//! (A pool cannot shrink under us: `ftruncate` of a mapped memfd is
//! `EBUSY` in this kernel.) Every protocol error sends `error` and
//! disconnects the client, as Wayland does.
//!
//! **Pointer lock** (Wayland's pointer-constraints, for games). A surface
//! asks with `lock_pointer`; the lock is *active* only while that surface
//! has the focus — it engages when the surface gets the focus or when a
//! click lands in its content, and ends when the focus goes elsewhere, the
//! surface goes away, the client gives it up, or the user presses
//! Ctrl+Alt (the way out, as in a virtual machine's window; the next click
//! in the window takes it back). While active the pointer does not move:
//! motion goes to the surface as `relative_motion`, and every button to it
//! too, with no title-bar dragging or focus changes.
//!
//! **Window management** (phase 4 of `docs/gui/gui-plan.md`).
//!
//! - *The frame has a size of its own.* A window's content box
//!   (`fw x fh`) is normally its buffer's size, but the compositor sets it
//!   when it maximizes or finishes a resize, sends `resize(fw, fh)`, and
//!   keeps showing the old content — the rest filled with
//!   [`WINDOW_BG`] — until the client answers. The answer is any buffer
//!   *created after* the `resize` was sent: its size becomes the frame's,
//!   whatever it is (a terminal rounds to whole cells), so the client has
//!   the last word. A buffer from before — a frame drawn before the
//!   client read the event — does not snap the frame back. This stands in
//!   for Wayland's `ack_configure`, which the protocol does without.
//! - *Decorations are drawn here*, not by clients: a title bar with the
//!   title, a close button and, for a resizable window, maximize. This
//!   crate decides the geometry and paints bars and buttons; the title's
//!   text is painted by the caller's closure ([`Compositor::compose_with`]),
//!   since this crate knows no fonts. Sizes scale with the screen
//!   ([`scale_for`]).
//! - *Only a window that sent `set_resizable` can be resized or
//!   maximized*: it gets an invisible border and a grip in its bottom-right
//!   corner. Dragging them draws an outline only; the release sends one
//!   `resize`. Double-click on the title bar toggles maximize.
//! - *F11* is this crate's key, not the client's: a window that sent
//!   `set_resizable` covers the whole screen with no title bar (and no
//!   resize grip) and is asked for the screen's size; pressed again it goes
//!   back to its placed or maximized geometry.
//! - *Close* sends `close`; the client decides. Nothing here kills.
//! - *The panel* (`set_panel`) is a strip along the bottom: undecorated,
//!   above every window, outside the work area (maximize and placement
//!   avoid it), never focused (a click in it leaves the focus where it
//!   was). It alone gets the window list, with ids of the compositor's own.

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use core::cell::RefCell;
use alloc::string::String;
use alloc::vec::Vec;

use crate::protocol::{DecodeError, ErrorCode, Event, Interface, Request, FORMAT_ARGB8888, FORMAT_XRGB8888, MAX_TITLE};
use crate::region::{Rect, Region};
use crate::theme::{self, Button, Shape, Theme};
use crate::wire::{Decoder, WireError};

pub type ClientId = u32;

/// A pool's memory as mapped by the host. Real ones are `mmap`s of a memfd
/// (unmapped on drop); tests hand out a `Vec`'s pointer.
pub trait PoolMem {
    fn as_ptr(&self) -> *const u8;
    fn len(&self) -> usize;
}

/// Title bar height at scale 1; see [`title_height_for`].
pub const TITLE_H: i32 = 20;
/// Where a window's content is not covered by its buffer (between a
/// `resize` and the client's answer), and where the CPU painter has a
/// GPU buffer to show (it cannot).
pub const WINDOW_BG: u32 = 0x0018_1818;
/// The resize outline.
pub const OUTLINE: u32 = 0x00E0_E0E0;
/// Largest surface side accepted.
pub const MAX_SIDE: i32 = 8192;
pub const MAX_OBJECTS: usize = 512;
/// Most rectangles `compose` reports (`FBIO_FLUSH` takes 16 per call).
pub const MAX_FLUSH_RECTS: usize = 16;
/// Two presses on a title bar closer than this are a double click.
pub const DOUBLE_CLICK_MS: u32 = 400;

pub const BTN_LEFT: u32 = 0x110;
const KEY_BACKSPACE: u32 = 14;
const KEY_ESC: u32 = 1;
/// How far past a popup its frame's shadow may reach, at scale 1: what is damaged around it when it shows or hides.
const POPUP_MARGIN: i32 = 24;
const KEY_F11: u32 = 87;
const KEY_F12: u32 = 88;
const KEY_LEFTCTRL: u32 = 29;
const KEY_RIGHTCTRL: u32 = 97;
const KEY_LEFTALT: u32 = 56;
const KEY_RIGHTALT: u32 = 100;

/// The factor decorations are drawn at on a screen `height` pixels tall —
/// 1 up to 1079, 2 at 1080p, 3 at 1620 and above: the same steps as a
/// `gfx::HIDPI` program's scale on that screen.
pub fn scale_for(height: i32) -> i32 {
    (height / 540).clamp(1, 3)
}

/// The title bar's height on a screen `height` pixels tall.
pub fn title_height_for(height: i32) -> i32 {
    TITLE_H * scale_for(height)
}

/// The software cursor: `X` black, `.` white, space transparent. Hotspot
/// at its top-left corner.
pub const CURSOR: [&[u8; 11]; 16] = [
    b"X          ",
    b"XX         ",
    b"X.X        ",
    b"X..X       ",
    b"X...X      ",
    b"X....X     ",
    b"X.....X    ",
    b"X......X   ",
    b"X.......X  ",
    b"X........X ",
    b"X.....XXXXX",
    b"X..X..X    ",
    b"X.X X..X   ",
    b"XX  X..X   ",
    b"X    X..X  ",
    b"     XXX   ",
];
pub const CURSOR_W: i32 = 11;
pub const CURSOR_H: i32 = 16;

/// Which sides of a window a resize drag moves.
pub const EDGE_LEFT: u8 = 1;
pub const EDGE_RIGHT: u8 = 2;
pub const EDGE_TOP: u8 = 4;
pub const EDGE_BOTTOM: u8 = 8;

/// What a point on screen is, for the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    Content,
    Title,
    Close,
    Maximize,
    /// A resize border or grip: `EDGE_*` bits.
    Edge(u8),
}

/// What [`Compositor::compose_with`] asks the caller to paint: a window's
/// title, left-aligned in `area` (screen coordinates, the bar's full
/// height), in `fg` (`0x00RRGGBB`) over what is already there (the bar),
/// with a shadow one pixel × scale down and right in `shadow`
/// (`0xAARRGGBB`, alpha 0 = none). `id` is the window's toplevel id,
/// stable while it is mapped — a cache key.
/// The closure that paints a title: `(title, clip, dst, stride)`.
pub type PaintTitle<'a> = dyn FnMut(&TitleText, Rect, &mut [u32], usize) + 'a;

pub struct TitleText<'a> {
    pub id: u32,
    pub title: &'a str,
    pub focused: bool,
    pub fg: u32,
    pub shadow: u32,
    pub area: Rect,
}

/// A buffer: a window into a pool. The pool's memory lives as long as any
/// buffer of it, even after the pool is destroyed (Wayland's rule).
struct BufRef<M> {
    id: u32,
    /// Creation order, compositor-wide: tells an answer to a `resize`
    /// from a buffer that predates it.
    serial: u64,
    mem: Rc<M>,
    offset: usize,
    w: i32,
    h: i32,
    stride: usize,
    /// `ARGB8888`: premultiplied alpha, shown "over".
    premul: bool,
}

impl<M> Clone for BufRef<M> {
    fn clone(&self) -> Self {
        BufRef {
            id: self.id,
            serial: self.serial,
            mem: self.mem.clone(),
            offset: self.offset,
            w: self.w,
            h: self.h,
            stride: self.stride,
            premul: self.premul,
        }
    }
}

/// What the host of a GPU compositor must do for the buffers clients create. Taken with [`Compositor::take_gpu_ops`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GpuOp {
    /// Make the descriptor `fd` (`size` bytes of a client's GPU buffer, rows `stride` bytes apart) readable as buffer `handle`. The host owns
    /// the descriptor from now on: it closes it, whether or not the import works.
    Import { handle: u64, fd: i32, size: usize, width: i32, height: i32, stride: usize },
    /// Nothing refers to `handle` any more. Free it once every frame that was started before this op is done: the GPU may still be reading it.
    /// Every `Import` is followed by exactly one `Drop`, even when the client was disconnected for the request.
    Drop { handle: u64 },
}

type GpuOps = Rc<RefCell<Vec<GpuOp>>>;

/// The compositor's reference to an imported buffer; the last one going away queues `GpuOp::Drop`.
struct GpuBuf {
    handle: u64,
    ops: GpuOps,
}

impl Drop for GpuBuf {
    fn drop(&mut self) {
        self.ops.borrow_mut().push(GpuOp::Drop { handle: self.handle });
    }
}

/// A GPU buffer object of a client.
#[derive(Clone)]
struct GpuRef {
    id: u32,
    serial: u64,
    w: i32,
    h: i32,
    buf: Rc<GpuBuf>,
}

/// What a surface has attached: a copy-at-commit buffer or a GPU one.
enum Attached<M> {
    Cpu(BufRef<M>),
    Gpu(GpuRef),
}

/// A replaced GPU buffer waiting for the frames that may have read it.
struct Retired {
    client: ClientId,
    buf: GpuRef,
    /// Released once frame `after` is done.
    after: u64,
}

/// One thing to draw, from [`Compositor::draw_list`]. Rectangles are in screen pixels and already clipped to the screen, except a
/// [`DrawOp::Shape`]'s.
#[derive(Clone, Debug, PartialEq)]
pub enum DrawOp {
    /// A solid `0x00RRGGBB` rectangle.
    Fill { rect: Rect, color: u32 },
    /// `dst` shows the GPU buffer `handle` (see [`GpuOp::Import`]) starting at pixel (`sx`, `sy`) of it, one pixel to one.
    Gpu { handle: u64, dst: Rect, sx: i32, sy: i32 },
    /// `dst` shows the pixels [`Compositor::cpu_content`] returns for (`client`, `surface`): `w x h`, from (`sx`, `sy`). `version` changes
    /// whenever the pixels do. `premul`: they are premultiplied `0xAARRGGBB`, drawn "over" ([`theme::over`]); else opaque.
    Cpu { client: ClientId, surface: u32, version: u64, dst: Rect, sx: i32, sy: i32, w: i32, h: i32, premul: bool },
    /// A window's title, to paint over its bar in `fg` (`0x00RRGGBB`) with a shadow one pixel × scale down and right in `shadow`
    /// (`0xAARRGGBB`, alpha 0 = none), left-aligned in `area` and touching only `clip`; transparent around the glyphs. `id` is stable while
    /// the window is mapped (a cache key).
    Title { id: u32, title: String, focused: bool, fg: u32, shadow: u32, area: Rect, clip: Rect },
    /// The box `rect` drawn as `shape` says (see [`theme`]), touching only `clip` if there is one (a title bar's box reaches under the
    /// content so that only its top corners are round; the clip keeps it out). Not clipped to the screen: the shape and its shadow may
    /// reach past it, the host clips.
    Shape { rect: Rect, shape: Shape, clip: Option<Rect> },
    /// The pointer, its hotspot at (`x`, `y`): the bitmap is [`CURSOR`].
    Cursor { x: i32, y: i32 },
}

/// A fill clipped to the screen.
/// `r` grown by `m` on every side.
fn grow(r: Rect, m: i32) -> Rect {
    Rect::new(r.x - m, r.y - m, r.w + 2 * m, r.h + 2 * m)
}

fn push_fill(ops: &mut Vec<DrawOp>, scr: Rect, rect: Rect, color: u32) {
    if let Some(rect) = rect.intersect(&scr) {
        ops.push(DrawOp::Fill { rect, color });
    }
}

struct Surface<M> {
    /// `None`: nothing attached since the last commit. `Some(None)`: a null
    /// attach (unmap at commit).
    pending_buffer: Option<Option<Attached<M>>>,
    /// The GPU buffer that is the content now (then `store` is empty).
    gpu: Option<GpuRef>,
    /// Counts the commits that changed the content: a host that keeps a copy of the store uploads it when this moves.
    version: u64,
    pending_damage: Region,
    pending_frames: Vec<u32>,
    title: String,
    /// Current content, `w x h`, row-major.
    store: Vec<u32>,
    w: i32,
    h: i32,
    /// The content box on screen, which a resize or maximize sets ahead of
    /// the client's buffer.
    fw: i32,
    fh: i32,
    /// A `resize` sent and not answered yet: the buffer serial at the time;
    /// only a buffer created after it answers.
    resize_pending: Option<u64>,
    mapped: bool,
    /// The client asked for the pointer (`lock_pointer`).
    wants_lock: bool,
    /// `set_resizable`'s minimum content size.
    resizable: Option<(i32, i32)>,
    /// Geometry (`x, y, fw, fh`) to go back to when unmaximized.
    maximized: Option<Rect>,
    /// Fullscreen (F11): the content box and the maximized state to go back to. While it lasts the window has no title bar and covers
    /// the whole screen.
    fullscreen: Option<(Rect, Option<Rect>)>,
    /// Has a title bar (every window but the panel and popups).
    decorated: bool,
    /// The popup role: shown at (x, y) of this other surface's content.
    popup: Option<(Key, i32, i32)>,
    /// The content is premultiplied `ARGB8888` (the last pool buffer committed was), shown "over" what is under it.
    premul: bool,
    /// Toplevel id while mapped as a window, 0 otherwise.
    tid: u32,
    /// Top-left of the window's frame (title bar included).
    x: i32,
    y: i32,
}

/// Frame geometry, from a surface and the title bar's height.
impl<M> Surface<M> {
    fn th(&self, th: i32) -> i32 {
        if self.decorated { th } else { 0 }
    }
    fn frame(&self, th: i32) -> Rect {
        Rect::new(self.x, self.y, self.fw, self.fh + self.th(th))
    }
    fn content(&self, th: i32) -> Rect {
        Rect::new(self.x, self.y + self.th(th), self.fw, self.fh)
    }
    fn title_bar(&self, th: i32) -> Rect {
        Rect::new(self.x, self.y, self.fw, self.th(th))
    }
    /// The close button, then maximize (resizable only), from the right.
    fn close_button(&self, th: i32) -> Rect {
        Rect::new(self.x + self.fw - th, self.y, th.min(self.fw), self.th(th))
    }
    fn max_button(&self, th: i32) -> Option<Rect> {
        self.resizable.map(|_| Rect::new(self.x + self.fw - 2 * th, self.y, th, self.th(th)))
    }
    fn buttons_left(&self, th: i32) -> i32 {
        self.x + self.fw - if self.resizable.is_some() { 2 * th } else { th }
    }
}

enum Object<M> {
    Pool { mem: Rc<M>, size: usize },
    Buffer(BufRef<M>),
    GpuBuffer(GpuRef),
    Surface(Surface<M>),
    Callback,
}

impl<M> Object<M> {
    fn interface(&self) -> Interface {
        match self {
            Object::Pool { .. } => Interface::Pool,
            Object::Buffer(_) | Object::GpuBuffer(_) => Interface::Buffer,
            Object::Surface(_) => Interface::Surface,
            Object::Callback => Interface::Callback,
        }
    }
}

struct Client<M> {
    decoder: Decoder,
    objects: BTreeMap<u32, Object<M>>,
}

type Key = (ClientId, u32);

struct Drag {
    key: Key,
    /// Pointer position minus the window's, when the drag began.
    dx: i32,
    dy: i32,
}

struct ResizeDrag {
    key: Key,
    edges: u8,
    /// Pointer where the drag began.
    px: i32,
    py: i32,
    /// Content box (`x, y` of the frame, `fw x fh`) when it began.
    start: Rect,
    /// The frame the outline shows now.
    outline: Rect,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ButtonKind {
    Close,
    Maximize,
}

pub struct Compositor<M> {
    width: i32,
    height: i32,
    scale: i32,
    th: i32,
    clients: BTreeMap<ClientId, Client<M>>,
    next_client: ClientId,
    /// Mapped windows, bottom to top. The panel is not in it.
    stack: Vec<Key>,
    /// The panel's surface and its height.
    panel: Option<(Key, i32)>,
    /// Mapped popups, bottom to top: above everything, the panel included.
    popups: Vec<Key>,
    /// Mapped windows by toplevel id, in the order they appeared.
    toplevels: Vec<(u32, Key)>,
    next_tid: u32,
    focus: Option<Key>,
    pointer: (i32, i32),
    drag: Option<Drag>,
    resize: Option<ResizeDrag>,
    /// A title-bar button held down.
    pressed: Option<(Key, ButtonKind)>,
    /// The last press on a title bar, for double clicks.
    last_title_press: Option<(Key, u32)>,
    now_ms: u32,
    /// Where a content-area press went, so its release goes there too.
    button_target: Option<Key>,
    /// The surface the pointer is locked to, if the lock is active.
    locked: Option<Key>,
    ctrl: u8,
    alt: u8,
    quit: bool,
    placed: i32,
    buffers_created: u64,
    damage: Region,
    frame_waiting: Vec<(ClientId, u32)>,
    events: Vec<(ClientId, Event)>,
    disconnects: Vec<ClientId>,
    fds_to_close: Vec<i32>,
    /// `create_gpu_buffer` is only for a host that composes on the GPU ([`Compositor::enable_gpu_buffers`]); the CPU painter cannot show one.
    gpu_enabled: bool,
    gpu_ops: GpuOps,
    next_gpu_handle: u64,
    retired: Vec<Retired>,
    /// The last frame handed out by `draw_list`, and the last the host said is done.
    epoch_issued: u64,
    epoch_done: u64,
    /// The draw list's look ([`theme`]); `compose` always paints [`theme::FLAT`].
    theme: &'static Theme,
}

impl<M: PoolMem> Compositor<M> {
    pub fn new(width: i32, height: i32) -> Self {
        let scale = scale_for(height);
        Compositor {
            width,
            height,
            scale,
            th: TITLE_H * scale,
            clients: BTreeMap::new(),
            next_client: 1,
            stack: Vec::new(),
            panel: None,
            popups: Vec::new(),
            toplevels: Vec::new(),
            next_tid: 1,
            focus: None,
            pointer: (width / 2, height / 2),
            drag: None,
            resize: None,
            pressed: None,
            last_title_press: None,
            now_ms: 0,
            button_target: None,
            locked: None,
            ctrl: 0,
            alt: 0,
            quit: false,
            placed: 0,
            buffers_created: 0,
            // The first compose paints everything.
            damage: Region::from_rect(Rect::new(0, 0, width, height)),
            frame_waiting: Vec::new(),
            events: Vec::new(),
            disconnects: Vec::new(),
            fds_to_close: Vec::new(),
            gpu_enabled: false,
            gpu_ops: Rc::new(RefCell::new(Vec::new())),
            next_gpu_handle: 1,
            retired: Vec::new(),
            epoch_issued: 0,
            epoch_done: 0,
            theme: theme::THEMES[0],
        }
    }

    fn screen(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    /// The screen minus the panel: where windows are placed and maximized.
    pub fn work_area(&self) -> Rect {
        let ph = self.panel.map_or(0, |(_, h)| h);
        Rect::new(0, 0, self.width, self.height - ph)
    }

    /// The title bar's height on this screen.
    pub fn title_height(&self) -> i32 {
        self.th
    }

    /// The draw list's look from now on (the whole screen is damaged). F12 cycles through [`theme::THEMES`].
    pub fn set_theme(&mut self, t: &'static Theme) {
        self.theme = t;
        self.damage.add(self.screen());
        self.tell_panel(|p| Event::Theme { surface: p, name: String::from(t.name) });
    }

    pub fn theme(&self) -> &'static Theme {
        self.theme
    }

    fn border(&self) -> i32 {
        4 * self.scale
    }

    fn grip(&self) -> i32 {
        12 * self.scale
    }

    /// The host's clock, in ms, for double clicks: call it before feeding
    /// input.
    pub fn set_time(&mut self, ms: u32) {
        self.now_ms = ms;
    }

    // ── clients ───────────────────────────────────────────────────────────

    pub fn add_client(&mut self) -> ClientId {
        let id = self.next_client;
        self.next_client += 1;
        self.clients.insert(id, Client { decoder: Decoder::new(), objects: BTreeMap::new() });
        id
    }

    pub fn has_client(&self, c: ClientId) -> bool {
        self.clients.contains_key(&c)
    }

    /// The client hung up (or is being dropped): its windows go away and
    /// any fds it sent that no request claimed are queued for closing.
    pub fn remove_client(&mut self, c: ClientId) {
        let Some(mut client) = self.clients.remove(&c) else { return };
        self.fds_to_close.extend(client.decoder.drain_fds());
        let (th, mg) = (self.th, self.decor_margin());
        for (id, obj) in &client.objects {
            if let Object::Surface(s) = obj {
                if s.mapped {
                    self.damage.add(grow(s.frame(th), mg));
                }
                self.forget_surface((c, *id));
            }
        }
        self.frame_waiting.retain(|(fc, _)| *fc != c);
        self.retired.retain(|r| r.client != c);
    }

    fn fail(&mut self, c: ClientId, object: u32, code: ErrorCode, message: &str) {
        self.events.push((c, Event::Error { object, code: code as u32, message: message.into() }));
        self.disconnects.push(c);
        self.remove_client(c);
    }

    /// Bytes and fds from one `recvmsg` of client `c`. Every whole message
    /// is handled; a partial one waits for the rest. `map(fd, size)` maps a
    /// new pool (the fd is closed afterwards whatever it returns).
    pub fn client_data(&mut self, c: ClientId, bytes: &[u8], fds: &[i32], map: &mut impl FnMut(i32, usize) -> Option<M>) {
        let Some(client) = self.clients.get_mut(&c) else {
            self.fds_to_close.extend_from_slice(fds);
            return;
        };
        client.decoder.push_bytes(bytes);
        client.decoder.push_fds(fds);
        loop {
            let Some(client) = self.clients.get_mut(&c) else { return };
            let msg = match client.decoder.next_message() {
                Ok(Some(m)) => m,
                Ok(None) => return,
                Err(_) => return self.fail(c, 0, ErrorCode::InvalidMethod, "malformed message"),
            };
            let Some(iface) = client.objects.get(&msg.object).map(Object::interface).or(
                if msg.object == crate::protocol::COMPOSITOR_ID { Some(Interface::Compositor) } else { None },
            ) else {
                return self.fail(c, msg.object, ErrorCode::InvalidObject, "no such object");
            };
            let req = match Request::decode(iface, &msg, &mut client.decoder) {
                Ok(r) => r,
                Err(DecodeError::UnknownOpcode) => return self.fail(c, msg.object, ErrorCode::InvalidMethod, "no such request"),
                Err(DecodeError::Wire(WireError::MissingFd)) => return self.fail(c, msg.object, ErrorCode::InvalidMethod, "fd missing"),
                Err(DecodeError::Wire(_)) => return self.fail(c, msg.object, ErrorCode::InvalidMethod, "bad arguments"),
            };
            self.handle(c, req, &mut *map);
        }
    }

    fn new_object(&mut self, c: ClientId, id: u32, obj: Object<M>) -> bool {
        let client = self.clients.get_mut(&c).expect("live client");
        if id <= crate::protocol::COMPOSITOR_ID || client.objects.contains_key(&id) {
            self.fail(c, id, ErrorCode::InvalidId, "id 0, 1 or in use");
            return false;
        }
        if client.objects.len() >= MAX_OBJECTS {
            self.fail(c, id, ErrorCode::NoMemory, "too many objects");
            return false;
        }
        client.objects.insert(id, obj);
        true
    }

    fn destroy_object(&mut self, c: ClientId, id: u32) {
        if let Some(client) = self.clients.get_mut(&c) {
            client.objects.remove(&id);
            self.events.push((c, Event::DeleteId { id }));
        }
    }

    fn surface_mut(&mut self, key: Key) -> Option<&mut Surface<M>> {
        match self.clients.get_mut(&key.0)?.objects.get_mut(&key.1)? {
            Object::Surface(s) => Some(s),
            _ => None,
        }
    }

    fn surface(&self, key: Key) -> Option<&Surface<M>> {
        match self.clients.get(&key.0)?.objects.get(&key.1)? {
            Object::Surface(s) => Some(s),
            _ => None,
        }
    }

    /// An event for the panel's surface, if there is a panel.
    fn tell_panel(&mut self, ev: impl FnOnce(u32) -> Event) {
        if let Some(((c, sid), _)) = self.panel {
            self.events.push((c, ev(sid)));
        }
    }

    fn handle(&mut self, c: ClientId, req: Request, map: &mut impl FnMut(i32, usize) -> Option<M>) {
        match req {
            Request::CreatePool { id, fd, size } => {
                let size = size as usize;
                let mem = map(fd, size);
                self.fds_to_close.push(fd);
                match mem {
                    Some(m) if size > 0 && m.len() >= size => {
                        self.new_object(c, id, Object::Pool { mem: Rc::new(m), size });
                    }
                    _ => self.fail(c, id, ErrorCode::BadPool, "cannot map pool"),
                }
            }
            Request::CreateSurface { id } => {
                let s = Surface {
                    pending_buffer: None,
                    gpu: None,
                    version: 0,
                    pending_damage: Region::new(),
                    pending_frames: Vec::new(),
                    title: String::new(),
                    store: Vec::new(),
                    w: 0,
                    h: 0,
                    fw: 0,
                    fh: 0,
                    resize_pending: None,
                    mapped: false,
                    wants_lock: false,
                    resizable: None,
                    maximized: None,
                    fullscreen: None,
                    decorated: true,
                    premul: false,
                    tid: 0,
                    popup: None,
                    x: 0,
                    y: 0,
                };
                if self.new_object(c, id, Object::Surface(s)) {
                    let (w, h) = ((self.width / 2).max(1), (self.height / 2).max(1));
                    self.events.push((c, Event::Configure { surface: id, width: w, height: h }));
                }
            }
            Request::Sync { id } => {
                if self.new_object(c, id, Object::Callback) {
                    self.events.push((c, Event::Done { callback: id, ms: 0 }));
                    self.destroy_object(c, id);
                }
            }
            Request::CreateGpuBuffer { id, fd, size, width, height, stride, format } => {
                let size = size as usize;
                if !self.gpu_enabled {
                    self.fds_to_close.push(fd);
                    return self.fail(c, crate::protocol::COMPOSITOR_ID, ErrorCode::InvalidMethod, "this compositor has no GPU buffers");
                }
                if format != FORMAT_XRGB8888 {
                    self.fds_to_close.push(fd);
                    return self.fail(c, crate::protocol::COMPOSITOR_ID, ErrorCode::InvalidFormat, "only XRGB8888");
                }
                let fits = width > 0
                    && height > 0
                    && width <= MAX_SIDE
                    && height <= MAX_SIDE
                    && stride >= width * 4
                    && stride % 4 == 0
                    && (stride as u64) * (height as u64 - 1) + (width as u64) * 4 <= size as u64;
                if !fits {
                    self.fds_to_close.push(fd);
                    return self.fail(c, crate::protocol::COMPOSITOR_ID, ErrorCode::InvalidBuffer, "buffer larger than its descriptor");
                }
                let handle = self.next_gpu_handle;
                self.next_gpu_handle += 1;
                self.buffers_created += 1;
                // The import is queued first: a rejected id (the client is dropped below) still gets its `Drop`, so the host sees pairs.
                self.gpu_ops.borrow_mut().push(GpuOp::Import { handle, fd, size, width, height, stride: stride as usize });
                let buf = Rc::new(GpuBuf { handle, ops: self.gpu_ops.clone() });
                self.new_object(c, id, Object::GpuBuffer(GpuRef { id, serial: self.buffers_created, w: width, h: height, buf }));
            }
            Request::CreateBuffer { pool, id, offset, width, height, stride, format } => {
                let Some(Object::Pool { mem, size }) = self.clients[&c].objects.get(&pool) else { unreachable!() };
                let (mem, size) = (mem.clone(), *size);
                if format != FORMAT_XRGB8888 && format != FORMAT_ARGB8888 {
                    return self.fail(c, pool, ErrorCode::InvalidFormat, "only XRGB8888 or ARGB8888");
                }
                let fits = width > 0
                    && height > 0
                    && width <= MAX_SIDE
                    && height <= MAX_SIDE
                    && offset >= 0
                    && stride >= width * 4
                    && (offset as u64) + (stride as u64) * (height as u64 - 1) + (width as u64) * 4 <= size as u64;
                if !fits {
                    return self.fail(c, pool, ErrorCode::InvalidBuffer, "buffer outside its pool");
                }
                self.buffers_created += 1;
                let b = BufRef {
                    id,
                    serial: self.buffers_created,
                    mem,
                    offset: offset as usize,
                    w: width,
                    h: height,
                    stride: stride as usize,
                    premul: format == FORMAT_ARGB8888,
                };
                self.new_object(c, id, Object::Buffer(b));
            }
            Request::DestroyPool { pool } | Request::DestroyBuffer { buffer: pool } => self.destroy_object(c, pool),
            Request::Attach { surface, buffer } => {
                let b = if buffer == 0 {
                    None
                } else {
                    match self.clients[&c].objects.get(&buffer) {
                        Some(Object::Buffer(b)) => Some(Attached::Cpu(b.clone())),
                        Some(Object::GpuBuffer(g)) => Some(Attached::Gpu(g.clone())),
                        _ => return self.fail(c, buffer, ErrorCode::InvalidObject, "not a buffer"),
                    }
                };
                self.surface_mut((c, surface)).unwrap().pending_buffer = Some(b);
            }
            Request::Damage { surface, x, y, w, h } => {
                self.surface_mut((c, surface)).unwrap().pending_damage.add(Rect::new(x, y, w, h));
            }
            Request::Frame { surface, id } => {
                if self.new_object(c, id, Object::Callback) {
                    self.surface_mut((c, surface)).unwrap().pending_frames.push(id);
                }
            }
            Request::Commit { surface } => self.commit((c, surface)),
            Request::SetTitle { surface, title } => {
                let mut t = title;
                if t.len() > MAX_TITLE {
                    let mut end = MAX_TITLE;
                    while !t.is_char_boundary(end) {
                        end -= 1;
                    }
                    t.truncate(end);
                }
                let th = self.th;
                let s = self.surface_mut((c, surface)).unwrap();
                s.title = t.clone();
                let (mapped, bar, tid) = (s.mapped, s.title_bar(th), s.tid);
                if mapped {
                    self.damage.add(bar);
                }
                if tid != 0 {
                    self.tell_panel(|p| Event::Toplevel { surface: p, id: tid, title: t });
                }
            }
            Request::LockPointer { surface, on } => {
                let key = (c, surface);
                self.surface_mut(key).unwrap().wants_lock = on;
                if !on && self.locked == Some(key) {
                    self.locked = None;
                } else if on && self.focus == Some(key) {
                    self.locked = Some(key);
                }
            }
            Request::SetResizable { surface, min_w, min_h } => {
                let th = self.th;
                let s = self.surface_mut((c, surface)).unwrap();
                s.resizable = Some((min_w.clamp(1, MAX_SIDE), min_h.clamp(1, MAX_SIDE)));
                let (mapped, bar) = (s.mapped, s.title_bar(th));
                if mapped {
                    self.damage.add(bar); // a maximize button appears
                }
            }
            Request::SetPanel { surface, height } => {
                let key = (c, surface);
                let taken = self.panel.is_some_and(|(k, _)| k != key);
                if taken || self.surface(key).unwrap().mapped || self.surface(key).unwrap().popup.is_some() {
                    return self.fail(c, surface, ErrorCode::Role, "panel role taken, or surface already a window or a popup");
                }
                let h = height.clamp(1, (self.height / 2).max(1));
                let s = self.surface_mut(key).unwrap();
                s.decorated = false;
                s.resizable = None;
                self.panel = Some((key, h));
                self.events.push((c, Event::Configure { surface, width: self.width, height: h }));
                for (tid, k) in self.toplevels.clone() {
                    let title = self.surface(k).map(|s| s.title.clone()).unwrap_or_default();
                    self.events.push((c, Event::Toplevel { surface, id: tid, title }));
                }
                let fid = self.focus.and_then(|k| self.surface(k)).map_or(0, |s| s.tid);
                self.events.push((c, Event::ToplevelFocus { surface, id: fid }));
                self.events.push((c, Event::Theme { surface, name: String::from(self.theme.name) }));
            }
            Request::SetPopup { surface, parent, x, y } => {
                let key = (c, surface);
                let s = self.surface(key).unwrap();
                let bad = parent == surface
                    || self.surface((c, parent)).is_none()
                    || s.mapped
                    || s.popup.is_some()
                    || self.panel.is_some_and(|(k, _)| k == key);
                if bad {
                    return self.fail(c, surface, ErrorCode::Role, "popup: no such parent, or the surface is mapped, a popup or the panel");
                }
                let s = self.surface_mut(key).unwrap();
                s.decorated = false;
                s.resizable = None;
                s.popup = Some(((c, parent), x, y));
            }
            Request::SetTheme { surface, name } => {
                if self.panel.is_some_and(|(k, _)| k == (c, surface)) {
                    if let Some(t) = theme::by_name(&name) {
                        self.set_theme(t);
                    }
                }
            }
            Request::Activate { surface, toplevel } => {
                if self.panel.is_some_and(|(k, _)| k == (c, surface)) {
                    if let Some(&(_, k)) = self.toplevels.iter().find(|(t, _)| *t == toplevel) {
                        self.raise(k);
                        self.set_focus(Some(k));
                    }
                }
            }
            Request::DestroySurface { surface } => {
                let key = (c, surface);
                let mg = self.decor_margin();
                if let Some(s) = self.surface(key) {
                    if s.mapped {
                        self.damage.add(grow(s.frame(self.th), mg));
                    }
                }
                self.forget_surface(key);
                self.destroy_object(c, surface);
            }
        }
    }

    /// Applies the pending state at once, as Wayland's `commit`.
    fn commit(&mut self, key: Key) {
        let (c, _) = key;
        let placed = self.placed;
        let th = self.th;
        let work = self.work_area();
        let panel = self.panel.filter(|(k, _)| *k == key).map(|(_, h)| h);
        let (sw, sh) = (self.width, self.height);
        let mg = self.decor_margin();
        // a popup: where its parent's content is
        let popup = self.surface(key).and_then(|s| s.popup).map(|(pk, x, y)| {
            let at = self.surface(pk).map_or(Rect::new(0, 0, 0, 0), |p| p.content(th));
            (at.x + x, at.y + y)
        });
        let s = self.surface_mut(key).unwrap();
        let frames = core::mem::take(&mut s.pending_frames);
        let damage = core::mem::take(&mut s.pending_damage);
        let mut screen_damage = Region::new();
        let mut released = None;
        let mut retire: Option<GpuRef> = None;
        let mut newly_mapped = false;
        let mut unmapped = false;
        match s.pending_buffer.take() {
            None => {}
            Some(None) => {
                if s.mapped {
                    screen_damage.add(grow(s.frame(th), mg));
                    s.mapped = false;
                    unmapped = true;
                }
                retire = s.gpu.take();
            }
            Some(Some(att)) => {
                let (bw, bh, bserial) = match &att {
                    Attached::Cpu(b) => (b.w, b.h, b.serial),
                    Attached::Gpu(g) => (g.w, g.h, g.serial),
                };
                let full = Region::from_rect(Rect::new(0, 0, bw, bh));
                let mut dmg = damage;
                if bw != s.w || bh != s.h {
                    s.w = bw;
                    s.h = bh;
                    dmg = full.clone();
                }
                match &att {
                    Attached::Cpu(b) => {
                        if s.store.len() != (bw * bh) as usize || s.premul != b.premul {
                            s.store = alloc::vec![0; (bw * bh) as usize];
                            s.premul = b.premul;
                            dmg = full.clone();
                        }
                    }
                    // the host reads the whole buffer every frame
                    Attached::Gpu(_) => {
                        s.store = Vec::new();
                        s.premul = false;
                        dmg = full.clone();
                    }
                }
                // The frame follows the buffer, except for one from before
                // our `resize` (see the module doc).
                let answers = s.resize_pending.is_none_or(|ser| bserial > ser);
                if answers {
                    s.resize_pending = None;
                    if (s.fw, s.fh) != (bw, bh) {
                        if s.mapped {
                            screen_damage.add(grow(s.frame(th), mg)); // the old frame
                        }
                        s.fw = bw;
                        s.fh = bh;
                        screen_damage.add(grow(s.frame(th), mg));
                    }
                }
                if !s.mapped {
                    dmg = full.clone();
                    if let Some(ph) = panel {
                        s.x = 0;
                        s.y = sh - ph;
                    } else if let Some((px, py)) = popup {
                        // where it was asked for, kept on the screen
                        s.x = px.min(sw - bw).max(0);
                        s.y = py.min(sh - bh).max(0);
                    } else {
                        // Cascade from the top-left, kept in the work area.
                        let step = 32 * (placed % 8);
                        s.x = (work.x + 40 + step).min((work.right() - bw).max(work.x));
                        s.y = (work.y + 40 + step).min((work.bottom() - bh - th).max(work.y));
                    }
                    s.mapped = true;
                    newly_mapped = true;
                }
                dmg.intersect(Rect::new(0, 0, bw, bh));
                match att {
                    Attached::Cpu(b) => {
                        copy_damage(&b, &mut s.store, &dmg);
                        retire = s.gpu.take();
                        released = Some(b.id);
                    }
                    Attached::Gpu(g) => {
                        // the same buffer committed again keeps its place; another one replaces (and retires) the old
                        if !s.gpu.as_ref().is_some_and(|o| Rc::ptr_eq(&o.buf, &g.buf)) {
                            retire = s.gpu.replace(g);
                        }
                    }
                }
                s.version += 1;
                let mut on_screen = dmg.clone();
                let content = s.content(th);
                on_screen.translate(content.x, content.y);
                screen_damage.add_region(&on_screen);
                if newly_mapped {
                    screen_damage.add(grow(s.frame(th), mg));
                }
            }
        }
        let title = s.title.clone();
        self.damage.add_region(&screen_damage);
        if let Some(g) = retire {
            self.retire_gpu(c, g);
        }
        if let Some(id) = released {
            self.events.push((c, Event::Release { buffer: id }));
        }
        self.frame_waiting.extend(frames.into_iter().map(|id| (c, id)));
        if newly_mapped && popup.is_some() {
            self.popups.push(key);
            let r = self.popup_extent(key);
            self.damage.add(r);
        }
        if newly_mapped && panel.is_none() && popup.is_none() {
            self.placed += 1;
            let tid = self.next_tid;
            self.next_tid += 1;
            self.surface_mut(key).unwrap().tid = tid;
            self.toplevels.push((tid, key));
            self.tell_panel(|p| Event::Toplevel { surface: p, id: tid, title });
            self.stack.push(key);
            self.set_focus(Some(key));
        }
        if unmapped {
            self.forget_surface(key);
        }
    }

    /// A GPU buffer stopped being a surface's content: tell its client it may reuse it, once the frames that may have read it are done.
    fn retire_gpu(&mut self, c: ClientId, buf: GpuRef) {
        if self.epoch_issued <= self.epoch_done {
            self.release_gpu(c, &buf);
        } else {
            self.retired.push(Retired { client: c, buf, after: self.epoch_issued });
        }
    }

    /// `release` for a GPU buffer, if the client still has that object (a destroyed one, whose id may be in use again, gets nothing).
    fn release_gpu(&mut self, c: ClientId, buf: &GpuRef) {
        let alive = matches!(
            self.clients.get(&c).and_then(|cl| cl.objects.get(&buf.id)),
            Some(Object::GpuBuffer(g)) if Rc::ptr_eq(&g.buf, &buf.buf)
        );
        if alive {
            self.events.push((c, Event::Release { buffer: buf.id }));
        }
    }

    /// The host finished frame `epoch` (as handed out by [`Compositor::draw_list`]; frames complete in order): the GPU buffers replaced
    /// before it was started are released to their clients.
    pub fn gpu_frame_done(&mut self, epoch: u64) {
        self.epoch_done = self.epoch_done.max(epoch);
        let done = self.epoch_done;
        let mut i = 0;
        while i < self.retired.len() {
            if self.retired[i].after <= done {
                let r = self.retired.remove(i);
                self.release_gpu(r.client, &r.buf);
            } else {
                i += 1;
            }
        }
    }

    /// Accept `create_gpu_buffer` (the host takes [`GpuOp`]s and composes from [`Compositor::draw_list`]). Off by default: a client that
    /// sends one to a compositor that paints on the CPU is disconnected, as for any request it does not have.
    pub fn enable_gpu_buffers(&mut self) {
        self.gpu_enabled = true;
    }

    /// What the GPU host has to do about buffers since the last call, in order.
    pub fn take_gpu_ops(&mut self) -> Vec<GpuOp> {
        core::mem::take(&mut *self.gpu_ops.borrow_mut())
    }

    /// Takes `key` out of the stack, the window list and every input role
    /// it had.
    fn forget_surface(&mut self, key: Key) {
        self.stack.retain(|k| *k != key);
        if self.popups.contains(&key) {
            let r = self.popup_extent(key);
            self.damage.add(r);
            self.popups.retain(|k| *k != key);
        }
        // its popups go with it
        let orphans: Vec<Key> = self.popups.iter().copied().filter(|k| self.surface(*k).is_some_and(|s| s.popup.is_some_and(|(p, _, _)| p == key))).collect();
        for k in orphans {
            self.dismiss_popup(k);
        }
        if let Some(i) = self.toplevels.iter().position(|(_, k)| *k == key) {
            let (tid, _) = self.toplevels.remove(i);
            if let Some(s) = self.surface_mut(key) {
                s.tid = 0;
            }
            self.tell_panel(|p| Event::ToplevelGone { surface: p, id: tid });
        }
        if self.panel.is_some_and(|(k, _)| k == key) {
            self.panel = None;
            if let Some(s) = self.surface_mut(key) {
                s.decorated = true;
            }
        }
        if self.drag.as_ref().is_some_and(|d| d.key == key) {
            self.drag = None;
        }
        if let Some(r) = self.resize.take_if(|r| r.key == key) {
            self.damage_outline(r.outline);
        }
        if self.pressed.is_some_and(|(k, _)| k == key) {
            self.pressed = None;
        }
        if self.button_target == Some(key) {
            self.button_target = None;
        }
        if self.locked == Some(key) {
            self.locked = None;
        }
        if self.focus == Some(key) {
            self.focus = None;
            let top = self.stack.last().copied();
            self.set_focus(top);
            if top.is_none() {
                self.tell_panel(|p| Event::ToplevelFocus { surface: p, id: 0 });
            }
        }
    }

    /// How far a window's decorations (its frame and the frame's shadow) reach past its frame: damaged with it.
    fn decor_margin(&self) -> i32 {
        let t = self.theme;
        let reach = |f: &Shape| f.shadow_blur as i32 + 1 + f.shadow_dx.abs().max(f.shadow_dy.abs());
        (t.frame_w + reach(&t.frame[0]).max(reach(&t.frame[1]))) * self.scale
    }

    /// What a popup covers on screen, with room for its frame's shadow.
    fn popup_extent(&self, key: Key) -> Rect {
        let m = POPUP_MARGIN * self.scale;
        self.surface(key).map_or(Rect::new(0, 0, 0, 0), |s| {
            let f = s.frame(self.th);
            Rect::new(f.x - m, f.y - m, f.w + 2 * m, f.h + 2 * m)
        })
    }

    /// Hides a mapped popup and tells its client (`popup_done`); it stays a popup and shows again at its next commit with a buffer.
    fn dismiss_popup(&mut self, key: Key) {
        if !self.popups.contains(&key) {
            return;
        }
        let r = self.popup_extent(key);
        self.damage.add(r);
        self.popups.retain(|k| *k != key);
        if let Some(s) = self.surface_mut(key) {
            s.mapped = false;
        }
        if self.button_target == Some(key) {
            self.button_target = None;
        }
        self.events.push((key.0, Event::PopupDone { surface: key.1 }));
    }

    /// Every popup, top first.
    fn dismiss_popups(&mut self) {
        for k in self.popups.clone().into_iter().rev() {
            self.dismiss_popup(k);
        }
    }

    /// Mapped popups, bottom to top.
    pub fn popups(&self) -> Vec<(ClientId, u32)> {
        self.popups.clone()
    }

    fn set_focus(&mut self, key: Option<Key>) {
        if self.focus == key {
            return;
        }
        let (th, mg) = (self.th, self.decor_margin());
        if let Some(old) = self.focus {
            if let Some(s) = self.surface(old) {
                // the frame changes colour too (and its shadow)
                self.damage.add(grow(s.frame(th), mg));
                self.events.push((old.0, Event::Focus { surface: old.1, focused: false }));
            }
        }
        self.focus = key;
        self.locked = None;
        let mut tid = 0;
        if let Some(new) = key {
            if let Some((frame, wants, t)) = self.surface(new).map(|s| (s.frame(th), s.wants_lock, s.tid)) {
                self.damage.add(grow(frame, mg));
                self.events.push((new.0, Event::Focus { surface: new.1, focused: true }));
                if wants {
                    self.locked = Some(new);
                }
                tid = t;
            }
        }
        self.tell_panel(|p| Event::ToplevelFocus { surface: p, id: tid });
    }

    fn raise(&mut self, key: Key) {
        if self.stack.last() != Some(&key) {
            self.stack.retain(|k| *k != key);
            self.stack.push(key);
            let mg = self.decor_margin();
            if let Some(s) = self.surface(key) {
                self.damage.add(grow(s.frame(self.th), mg));
            }
        }
    }

    /// Sends `done` to every frame callback committed since the last call
    /// — call it right after flushing what `compose` returned.
    pub fn frame_done(&mut self, ms: u32) {
        for (c, id) in core::mem::take(&mut self.frame_waiting) {
            if self.clients.get(&c).is_some_and(|cl| cl.objects.contains_key(&id)) {
                self.events.push((c, Event::Done { callback: id, ms }));
                self.destroy_object(c, id);
            }
        }
    }

    pub fn has_frame_callbacks(&self) -> bool {
        !self.frame_waiting.is_empty()
    }

    // ── window management ─────────────────────────────────────────────────

    /// Moves and sizes a window's content box, repainting both places.
    fn set_geometry(&mut self, key: Key, g: Rect) {
        let (th, mg) = (self.th, self.decor_margin());
        let s = self.surface_mut(key).unwrap();
        let before = s.frame(th);
        s.x = g.x;
        s.y = g.y;
        s.fw = g.w;
        s.fh = g.h;
        let after = s.frame(th);
        self.damage.add(grow(before, mg));
        self.damage.add(grow(after, mg));
    }

    /// Asks the client for a `w x h` content — unless that is exactly
    /// what its buffer already is.
    fn request_size(&mut self, key: Key, w: i32, h: i32) {
        let serial = self.buffers_created;
        let s = self.surface_mut(key).unwrap();
        if (s.w, s.h) == (w, h) {
            s.resize_pending = None;
        } else {
            s.resize_pending = Some(serial);
            self.events.push((key.0, Event::Resize { surface: key.1, width: w, height: h }));
        }
    }

    fn toggle_maximize(&mut self, key: Key) {
        let th = self.th;
        let work = self.work_area();
        let s = self.surface_mut(key).unwrap();
        if s.resizable.is_none() {
            return;
        }
        let g = match s.maximized.take() {
            Some(saved) => saved,
            None => {
                s.maximized = Some(Rect::new(s.x, s.y, s.fw, s.fh));
                Rect::new(work.x, work.y, work.w, (work.h - th).max(1))
            }
        };
        let maximized = s.maximized;
        self.set_geometry(key, g);
        self.surface_mut(key).unwrap().maximized = maximized;
        self.request_size(key, g.w, g.h);
    }

    /// F11: the window covers the whole screen without its title bar, and goes back to what it was (placed or maximized) the next time.
    /// Only a window that sent `set_resizable` can, like maximize: it is how a client says it can take any size.
    fn toggle_fullscreen(&mut self, key: Key) {
        let (sw, sh) = (self.width, self.height);
        let (th, mg) = (self.th, self.decor_margin());
        let Some(s) = self.surface_mut(key) else { return };
        if s.resizable.is_none() || !s.mapped {
            return;
        }
        let before = s.frame(th);
        let g = match s.fullscreen.take() {
            Some((saved, maximized)) => {
                s.decorated = true;
                s.maximized = maximized;
                saved
            }
            None => {
                s.fullscreen = Some((Rect::new(s.x, s.y, s.fw, s.fh), s.maximized.take()));
                s.decorated = false;
                Rect::new(0, 0, sw, sh)
            }
        };
        s.x = g.x;
        s.y = g.y;
        s.fw = g.w;
        s.fh = g.h;
        let after = s.frame(th);
        self.damage.add(grow(before, mg));
        self.damage.add(grow(after, mg));
        self.request_size(key, g.w, g.h);
    }

    /// The content box a resize drag gives for the pointer at `(x, y)`,
    /// kept between the client's minimum and the work area.
    fn resize_geometry(&self, r: &ResizeDrag, x: i32, y: i32) -> Rect {
        let (min_w, min_h) = self.surface(r.key).and_then(|s| s.resizable).unwrap_or((1, 1));
        let work = self.work_area();
        let (max_w, max_h) = (work.w.min(MAX_SIDE).max(min_w), (work.h - self.th).min(MAX_SIDE).max(min_h));
        let (dx, dy) = (x - r.px, y - r.py);
        let s = r.start;
        let mut w = s.w;
        let mut h = s.h;
        if r.edges & EDGE_RIGHT != 0 {
            w += dx;
        } else if r.edges & EDGE_LEFT != 0 {
            w -= dx;
        }
        if r.edges & EDGE_BOTTOM != 0 {
            h += dy;
        } else if r.edges & EDGE_TOP != 0 {
            h -= dy;
        }
        let w = w.clamp(min_w, max_w);
        let h = h.clamp(min_h, max_h);
        let nx = if r.edges & EDGE_LEFT != 0 { s.x + s.w - w } else { s.x };
        let ny = if r.edges & EDGE_TOP != 0 { s.y + s.h - h } else { s.y };
        Rect::new(nx, ny, w, h)
    }

    fn outline_edges(&self, f: Rect) -> [Rect; 4] {
        let t = 2 * self.scale;
        [
            Rect::new(f.x, f.y, f.w, t),
            Rect::new(f.x, f.bottom() - t, f.w, t),
            Rect::new(f.x, f.y, t, f.h),
            Rect::new(f.right() - t, f.y, t, f.h),
        ]
    }

    fn damage_outline(&mut self, f: Rect) {
        for e in self.outline_edges(f) {
            self.damage.add(e);
        }
    }

    // ── input ─────────────────────────────────────────────────────────────

    fn cursor_rect(&self) -> Rect {
        Rect::new(self.pointer.0, self.pointer.1, CURSOR_W, CURSOR_H)
    }

    /// The window (or panel) under a point, and which part of it.
    pub fn hit(&self, x: i32, y: i32) -> Option<((ClientId, u32), Zone)> {
        let th = self.th;
        for &k in self.popups.iter().rev() {
            if self.surface(k).is_some_and(|s| s.content(th).contains(x, y)) {
                return Some((k, Zone::Content));
            }
        }
        if let Some((k, _)) = self.panel {
            if self.surface(k).is_some_and(|s| s.mapped && s.content(th).contains(x, y)) {
                return Some((k, Zone::Content));
            }
        }
        let (b, g) = (self.border(), self.grip());
        for &k in self.stack.iter().rev() {
            let Some(s) = self.surface(k) else { continue };
            let f = s.frame(th);
            let resizable = s.resizable.is_some() && s.fullscreen.is_none(); // a fullscreen window has no grip: its corner is the game's
            if f.contains(x, y) {
                if resizable && s.content(th).contains(x, y) && x >= f.right() - g && y >= f.bottom() - g {
                    return Some((k, Zone::Edge(EDGE_RIGHT | EDGE_BOTTOM)));
                }
                if !s.title_bar(th).contains(x, y) {
                    return Some((k, Zone::Content));
                }
                if s.close_button(th).contains(x, y) {
                    return Some((k, Zone::Close));
                }
                if s.max_button(th).is_some_and(|m| m.contains(x, y)) {
                    return Some((k, Zone::Maximize));
                }
                return Some((k, Zone::Title));
            }
            let outer = Rect::new(f.x - b, f.y - b, f.w + 2 * b, f.h + 2 * b);
            if resizable && outer.contains(x, y) {
                // The side(s) the pointer is beyond, plus the neighbouring
                // side when it is near that corner.
                let mut e = 0;
                if x < f.x {
                    e |= EDGE_LEFT;
                } else if x >= f.right() {
                    e |= EDGE_RIGHT;
                }
                if y < f.y {
                    e |= EDGE_TOP;
                } else if y >= f.bottom() {
                    e |= EDGE_BOTTOM;
                }
                if e & (EDGE_LEFT | EDGE_RIGHT) == 0 {
                    if x < f.x + g {
                        e |= EDGE_LEFT;
                    } else if x >= f.right() - g {
                        e |= EDGE_RIGHT;
                    }
                }
                if e & (EDGE_TOP | EDGE_BOTTOM) == 0 {
                    if y < f.y + g {
                        e |= EDGE_TOP;
                    } else if y >= f.bottom() - g {
                        e |= EDGE_BOTTOM;
                    }
                }
                return Some((k, Zone::Edge(e)));
            }
        }
        None
    }

    pub fn pointer_motion(&mut self, dx: i32, dy: i32) {
        if let Some(k) = self.locked {
            if dx != 0 || dy != 0 {
                self.events.push((k.0, Event::RelativeMotion { surface: k.1, dx, dy }));
            }
            return;
        }
        let old = self.cursor_rect();
        let x = (self.pointer.0 + dx).clamp(0, self.width - 1);
        let y = (self.pointer.1 + dy).clamp(0, self.height - 1);
        if (x, y) == self.pointer {
            return;
        }
        self.pointer = (x, y);
        self.damage.add(old);
        self.damage.add(self.cursor_rect());

        if let Some(r) = &self.resize {
            let g = self.resize_geometry(r, x, y);
            let outline = Rect::new(g.x, g.y, g.w, g.h + self.th);
            let before = r.outline;
            if outline != before {
                self.damage_outline(before);
                self.damage_outline(outline);
                self.resize.as_mut().unwrap().outline = outline;
            }
            return;
        }
        if let Some(d) = &self.drag {
            let (key, nx, ny) = (d.key, x - d.dx, y - d.dy);
            let s = self.surface_mut(key).unwrap();
            s.maximized = None; // moved: no longer the work area
            let g = Rect::new(nx, ny, s.fw, s.fh);
            self.set_geometry(key, g);
            return;
        }
        let target = self.button_target.or_else(|| self.hit(x, y).filter(|(_, z)| *z == Zone::Content).map(|(k, _)| k));
        if let Some(k) = target {
            let c = self.surface(k).unwrap().content(self.th);
            self.events.push((k.0, Event::Motion { surface: k.1, x: x - c.x, y: y - c.y }));
        }
    }

    pub fn pointer_button(&mut self, code: u32, pressed: bool) {
        let (x, y) = self.pointer;
        if !pressed {
            if code == BTN_LEFT {
                if self.drag.take().is_some() {
                    return;
                }
                if let Some(r) = self.resize.take() {
                    self.damage_outline(r.outline);
                    let g = self.resize_geometry(&r, x, y);
                    if g != r.start {
                        self.surface_mut(r.key).unwrap().maximized = None;
                        self.set_geometry(r.key, g);
                        self.request_size(r.key, g.w, g.h);
                    }
                    return;
                }
                if let Some((k, which)) = self.pressed.take() {
                    let th = self.th;
                    let bar = self.surface(k).unwrap().title_bar(th);
                    self.damage.add(bar);
                    let zone = if which == ButtonKind::Close { Zone::Close } else { Zone::Maximize };
                    if self.hit(x, y) == Some((k, zone)) {
                        match which {
                            ButtonKind::Close => self.events.push((k.0, Event::Close { surface: k.1 })),
                            ButtonKind::Maximize => self.toggle_maximize(k),
                        }
                    }
                    return;
                }
            }
            if let Some(k) = self.button_target.take() {
                self.events.push((k.0, Event::Button { surface: k.1, code, pressed: false }));
            }
            return;
        }
        if let Some(k) = self.locked {
            self.button_target = Some(k);
            self.events.push((k.0, Event::Button { surface: k.1, code, pressed: true }));
            return;
        }
        if self.drag.is_some() || self.resize.is_some() || self.pressed.is_some() {
            return; // another button during a left-button gesture
        }
        if !self.popups.is_empty() && !self.hit(x, y).is_some_and(|(k, _)| self.popups.contains(&k)) {
            // a click outside the popups closes them, and goes nowhere
            self.dismiss_popups();
            return;
        }
        let Some((k, zone)) = self.hit(x, y) else { return };
        if self.panel.is_some_and(|(p, _)| p == k) || self.popups.contains(&k) {
            // The panel and popups take clicks, never the focus.
            self.button_target = Some(k);
            self.events.push((k.0, Event::Button { surface: k.1, code, pressed: true }));
            return;
        }
        self.raise(k);
        self.set_focus(Some(k));
        let th = self.th;
        let s = self.surface(k).unwrap();
        let (wants_lock, resizable, bar) = (s.wants_lock, s.resizable.is_some(), s.title_bar(th));
        let (start, outline) = (Rect::new(s.x, s.y, s.fw, s.fh), s.frame(th));
        match zone {
            Zone::Content => {
                self.button_target = Some(k);
                self.events.push((k.0, Event::Button { surface: k.1, code, pressed: true }));
                if wants_lock {
                    self.locked = Some(k);
                }
            }
            _ if code != BTN_LEFT => {}
            Zone::Title => {
                let now = self.now_ms;
                let double = self.last_title_press.is_some_and(|(pk, t)| pk == k && now.wrapping_sub(t) <= DOUBLE_CLICK_MS);
                if double && resizable {
                    self.last_title_press = None;
                    self.toggle_maximize(k);
                } else {
                    self.last_title_press = Some((k, now));
                    self.drag = Some(Drag { key: k, dx: x - start.x, dy: y - start.y });
                }
            }
            Zone::Close | Zone::Maximize => {
                let which = if zone == Zone::Close { ButtonKind::Close } else { ButtonKind::Maximize };
                self.pressed = Some((k, which));
                self.damage.add(bar);
            }
            Zone::Edge(edges) => {
                self.resize = Some(ResizeDrag { key: k, edges, px: x, py: y, start, outline });
                self.damage_outline(outline);
            }
        }
    }

    /// An evdev key. Ctrl+Alt+Backspace sets [`Compositor::quit_requested`]
    /// instead of reaching a client.
    pub fn key(&mut self, code: u32, pressed: bool) {
        let adjust = |n: &mut u8| *n = if pressed { n.saturating_add(1) } else { n.saturating_sub(1) };
        match code {
            KEY_LEFTCTRL | KEY_RIGHTCTRL => adjust(&mut self.ctrl),
            KEY_LEFTALT | KEY_RIGHTALT => adjust(&mut self.alt),
            _ => {}
        }
        if pressed && code == KEY_BACKSPACE && self.ctrl > 0 && self.alt > 0 {
            self.quit = true;
            return;
        }
        if code == KEY_ESC && !self.popups.is_empty() {
            if pressed {
                self.dismiss_popups();
            }
            return;
        }
        if code == KEY_F12 {
            // the compositor's own key too: the next look
            if pressed {
                let i = theme::THEMES.iter().position(|t| core::ptr::eq(*t, self.theme)).map_or(0, |i| (i + 1) % theme::THEMES.len());
                self.set_theme(theme::THEMES[i]);
            }
            return;
        }
        if code == KEY_F11 {
            // the compositor's own key: not the client's, press or release
            if pressed {
                if let Some(k) = self.focus {
                    self.toggle_fullscreen(k);
                }
            }
            return;
        }
        let modifier = matches!(code, KEY_LEFTCTRL | KEY_RIGHTCTRL | KEY_LEFTALT | KEY_RIGHTALT);
        if pressed && modifier && self.ctrl > 0 && self.alt > 0 {
            self.locked = None; // the way out of a pointer lock
        }
        if let Some(k) = self.focus {
            self.events.push((k.0, Event::Key { surface: k.1, code, pressed }));
        }
    }

    pub fn quit_requested(&self) -> bool {
        self.quit
    }

    // ── output ────────────────────────────────────────────────────────────

    pub fn take_events(&mut self) -> Vec<(ClientId, Event)> {
        core::mem::take(&mut self.events)
    }

    pub fn take_disconnects(&mut self) -> Vec<ClientId> {
        core::mem::take(&mut self.disconnects)
    }

    pub fn take_fds_to_close(&mut self) -> Vec<i32> {
        core::mem::take(&mut self.fds_to_close)
    }

    pub fn has_damage(&self) -> bool {
        !self.damage.is_empty()
    }

    /// [`Compositor::compose_with`] with no title text.
    pub fn compose(&mut self, dst: &mut [u32], stride: usize) -> Vec<Rect> {
        self.compose_with(dst, stride, &mut |_, _, _, _| {})
    }

    /// Repaints everything damaged into `dst` (`stride` pixels per row, at
    /// least `width x height`) and returns the rectangles to flush — at
    /// most [`MAX_FLUSH_RECTS`], coarsened to a bounding box beyond that.
    /// `paint_title(title, clip, dst, stride)` draws a title's text over
    /// its bar, touching only `clip` (inside `title.area`).
    pub fn compose_with(
        &mut self,
        dst: &mut [u32],
        stride: usize,
        paint_title: &mut PaintTitle,
    ) -> Vec<Rect> {
        self.damage.intersect(self.screen());
        if self.damage.is_empty() || stride < self.width as usize || dst.len() < stride * self.height as usize {
            return Vec::new();
        }
        let ops = self.ops();
        for r in self.damage.rects().to_vec() {
            self.raster(&ops, r, dst, stride, paint_title);
        }
        let out = self.damage.coarsened(MAX_FLUSH_RECTS);
        self.damage.clear();
        out
    }

    // ── the draw list (GPU compositors) ───────────────────────────────────

    /// Everything on screen as drawing operations, back to front, for a host that composes on the GPU: the whole screen every time (a GPU
    /// does not mind), clipped to it. Returns the frame's number, which the host gives back to [`Compositor::gpu_frame_done`] when the GPU is
    /// done with the frame, and clears the damage. [`Compositor::compose`] paints the same list in software. Only the title text, the
    /// cursor's bitmap and the pixels of buffers are left to the host.
    pub fn draw_list(&mut self) -> (u64, Vec<DrawOp>) {
        self.epoch_issued += 1;
        let ops = self.ops();
        self.damage.clear();
        (self.epoch_issued, ops)
    }

    /// The screen as drawing operations, back to front (what [`Compositor::draw_list`] hands out and `compose` rasterises).
    fn ops(&self) -> Vec<DrawOp> {
        let scr = self.screen();
        let mut ops = Vec::new();
        ops.push(DrawOp::Shape { rect: scr, shape: self.theme.background.scaled(self.scale), clip: None });
        for &key in &self.stack {
            self.ops_surface(key, scr, &mut ops);
        }
        if let Some((k, _)) = self.panel {
            if let Some(s) = self.surface(k).filter(|s| s.mapped) {
                ops.push(DrawOp::Shape { rect: s.content(self.th), shape: self.theme.taskbar.bar.scaled(self.scale), clip: None });
                self.ops_surface(k, scr, &mut ops);
            }
        }
        for &k in &self.popups {
            ops.push(DrawOp::Shape { rect: self.surface(k).unwrap().content(self.th), shape: self.theme.menu.frame.scaled(self.scale), clip: None });
            self.ops_surface(k, scr, &mut ops);
        }
        if let Some(rs) = &self.resize {
            for e in self.outline_edges(rs.outline) {
                push_fill(&mut ops, scr, e, OUTLINE);
            }
        }
        if self.cursor_rect().intersect(&scr).is_some() {
            ops.push(DrawOp::Cursor { x: self.pointer.0, y: self.pointer.1 });
        }
        ops
    }

    /// The pixels of a window that is not a GPU buffer (`w x h`, rows `w` pixels long), for a host to upload when its `version`
    /// (in [`DrawOp::Cpu`]) changes.
    pub fn cpu_content(&self, client: ClientId, surface: u32) -> Option<&[u32]> {
        let s = self.surface((client, surface))?;
        (!s.store.is_empty()).then_some(&s.store[..])
    }

    /// [`Compositor::paint_surface`] as operations, unclipped by damage (`scr` clips).
    fn ops_surface(&self, key: Key, scr: Rect, ops: &mut Vec<DrawOp>) {
        let th = self.th;
        let s = self.surface(key).unwrap();
        if s.decorated {
            self.ops_decorations(key, scr, ops);
        }
        let content = s.content(th);
        if let Some(i) = content.intersect(&scr) {
            let shown = Rect::new(content.x, content.y, s.w.min(s.fw), s.h.min(s.fh));
            let covered = shown.intersect(&i);
            let mut rest = Region::from_rect(i);
            if let Some(cv) = covered {
                rest.subtract(cv);
            }
            for f in rest.rects() {
                push_fill(ops, scr, *f, WINDOW_BG);
            }
            if let Some(cv) = covered {
                let (sx, sy) = (cv.x - content.x, cv.y - content.y);
                if let Some(g) = &s.gpu {
                    ops.push(DrawOp::Gpu { handle: g.buf.handle, dst: cv, sx, sy });
                } else if !s.store.is_empty() {
                    ops.push(DrawOp::Cpu { client: key.0, surface: key.1, version: s.version, dst: cv, sx, sy, w: s.w, h: s.h, premul: s.premul });
                }
            }
        }
    }

    /// The title's text over the bar (`bar`: the part on screen).
    fn ops_title(&self, key: Key, bar: Rect, ops: &mut Vec<DrawOp>) {
        let th = self.th;
        let s = self.surface(key).unwrap();
        let focused = self.focus == Some(key);
        let pad = 6 * self.scale;
        let left = s.x + pad;
        let area = Rect::new(left, s.y, (s.buttons_left(th) - pad - left).max(0), th);
        if let Some(clip) = area.intersect(&bar) {
            let t = self.theme;
            let fg = t.title_fg[if focused { 0 } else { 1 }];
            ops.push(DrawOp::Title { id: s.tid, title: s.title.clone(), focused, fg, shadow: t.title_shadow, area, clip });
        }
    }

    /// A decorated window's frame, title bar and buttons in a theme made of shapes. The geometry is the flat look's (the bar is `th` tall,
    /// the buttons are `th` squares at its right end); the frame reaches `frame_w` past the window on every side.
    fn ops_decorations(&self, key: Key, scr: Rect, ops: &mut Vec<DrawOp>) {
        let (th, sc) = (self.th, self.scale);
        let t = self.theme;
        let s = self.surface(key).unwrap();
        let focused = self.focus == Some(key);
        let fi = if focused { 0 } else { 1 };
        let fw = t.frame_w * sc;
        let outer = s.frame(th);
        let outer = Rect::new(outer.x - fw, outer.y - fw, outer.w + 2 * fw, outer.h + 2 * fw);
        // the frame in two: its shadow (a square box with no fill, so the shadow is masked under the whole window), and its ring, square,
        // below the bar (whose rounded top corners are the window's): neither shows through a translucent window
        let f = t.frame[fi].scaled(sc);
        let shadow = Shape { c: [0; 4], border: 0.0, radius: 0.0, ..f };
        let ring = Shape { radius: 0.0, shadow_color: 0, ..f };
        ops.push(DrawOp::Shape { rect: outer, shape: shadow, clip: None });
        let below = Rect::new(outer.x, outer.y + fw + th, outer.w, outer.h - fw - th);
        ops.push(DrawOp::Shape { rect: outer, shape: ring, clip: Some(below) });
        let bar = t.title[fi].scaled(sc);
        // the box reaches under the content by the radius, so only the top corners show rounded
        let r = bar.radius as i32 + 1;
        let clip = Rect::new(outer.x, outer.y, outer.w, fw + th);
        ops.push(DrawOp::Shape { rect: Rect::new(outer.x, outer.y, outer.w, fw + th + r), shape: bar, clip: Some(clip) });
        let Some(tb) = s.title_bar(th).intersect(&scr) else { return };
        self.ops_title(key, tb, ops);
        let pressed = self.pressed.filter(|(k, _)| *k == key).map(|(_, b)| b);
        let mut buttons = alloc::vec![(s.close_button(th), t.close, pressed == Some(ButtonKind::Close), true)];
        if let Some(m) = s.max_button(th) {
            buttons.push((m, t.other, pressed == Some(ButtonKind::Maximize), false));
        }
        for (hit, style, down, close) in buttons {
            let i = t.button_inset * sc;
            let b = Rect::new(hit.x + i, hit.y + i, hit.w - 2 * i, hit.h - 2 * i);
            if b.w <= 0 || b.h <= 0 {
                continue;
            }
            match style {
                Button::Shape { normal, pressed } => {
                    ops.push(DrawOp::Shape { rect: b, shape: if down { pressed } else { normal }.scaled(sc), clip: None });
                }
                Button::Bevel { face, light, dark } => {
                    let (tl, br) = if down { (dark, light) } else { (light, dark) };
                    push_fill(ops, scr, b, br);
                    push_fill(ops, scr, Rect::new(b.x, b.y, b.w - sc, b.h - sc), tl);
                    push_fill(ops, scr, Rect::new(b.x + sc, b.y + sc, b.w - 2 * sc, b.h - 2 * sc), face);
                }
            }
            let w = t.glyph_weight * sc;
            if close {
                self.ops_glyph_x_w(b, w, t.glyph, scr, ops);
            } else {
                self.ops_glyph_square_w(b, w, t.glyph, scr, ops);
            }
        }
    }

    /// A × of stroke `w` in `color` inside `b`.
    fn ops_glyph_x_w(&self, b: Rect, w: i32, color: u32, scr: Rect, ops: &mut Vec<DrawOp>) {
        let (x0, y0, side) = self.glyph_box(b);
        for i in 0..(side - w + 1).max(1) {
            for (px, py) in [(x0 + i, y0 + i), (x0 + side - w - i, y0 + i)] {
                if let Some(p) = Rect::new(px, py, w, w).intersect(&b) {
                    push_fill(ops, scr, p, color);
                }
            }
        }
    }

    /// A maximize square of stroke `w` (twice that on top, as Windows draws it) in `color` inside `b`.
    fn ops_glyph_square_w(&self, b: Rect, w: i32, color: u32, scr: Rect, ops: &mut Vec<DrawOp>) {
        let (x0, y0, side) = self.glyph_box(b);
        for e in [
            Rect::new(x0, y0, side, 2 * w),
            Rect::new(x0, y0 + side - w, side, w),
            Rect::new(x0, y0, w, side),
            Rect::new(x0 + side - w, y0, w, side),
        ] {
            if let Some(p) = e.intersect(&b) {
                push_fill(ops, scr, p, color);
            }
        }
    }

    /// The side of a button glyph's box and its top-left, centred in `b`.
    fn glyph_box(&self, b: Rect) -> (i32, i32, i32) {
        let side = (self.th * 2 / 5).max(3);
        (b.x + (b.w - side) / 2, b.y + (b.h - side) / 2, side)
    }

    /// Paints `ops` into `dst` (rows `stride` long), touching only `r`: the software twin of a GPU host. A GPU buffer cannot be shown
    /// here: its place is [`WINDOW_BG`].
    fn raster(&self, ops: &[DrawOp], r: Rect, dst: &mut [u32], stride: usize, paint_title: &mut PaintTitle) {
        for op in ops {
            match op {
                DrawOp::Fill { rect, color } => {
                    if let Some(i) = rect.intersect(&r) {
                        fill(dst, stride, i, *color);
                    }
                }
                DrawOp::Shape { rect, shape, clip } => {
                    if let Some(c) = clip.map_or(Some(r), |c| c.intersect(&r)) {
                        shape.paint(dst, stride, c, *rect);
                    }
                }
                DrawOp::Gpu { dst: d, .. } => {
                    if let Some(i) = d.intersect(&r) {
                        fill(dst, stride, i, WINDOW_BG);
                    }
                }
                DrawOp::Cpu { client, surface, dst: d, sx, sy, w, premul, .. } => {
                    let (Some(i), Some(px)) = (d.intersect(&r), self.cpu_content(*client, *surface)) else { continue };
                    for py in i.y..i.bottom() {
                        let (row_y, row_x) = ((sy + py - d.y) as usize, (sx + i.x - d.x) as usize);
                        let src = &px[row_y * *w as usize + row_x..][..i.w as usize];
                        let row = &mut dst[py as usize * stride + i.x as usize..][..i.w as usize];
                        if *premul {
                            for (d, v) in row.iter_mut().zip(src) {
                                *d = theme::over(*d, *v);
                            }
                        } else {
                            row.copy_from_slice(src);
                        }
                    }
                }
                DrawOp::Title { id, title, focused, fg, shadow, area, clip } => {
                    if let Some(c) = clip.intersect(&r) {
                        let t = TitleText { id: *id, title, focused: *focused, fg: *fg, shadow: *shadow, area: *area };
                        paint_title(&t, c, dst, stride);
                    }
                }
                DrawOp::Cursor { x, y } => {
                    let Some(i) = Rect::new(*x, *y, CURSOR_W, CURSOR_H).intersect(&r) else { continue };
                    for py in i.y..i.bottom() {
                        let row = CURSOR[(py - y) as usize];
                        for px in i.x..i.right() {
                            let v = match row[(px - x) as usize] {
                                b'X' => 0x0000_0000,
                                b'.' => 0x00FF_FFFF,
                                _ => continue,
                            };
                            dst[py as usize * stride + px as usize] = v;
                        }
                    }
                }
            }
        }
        // the screen is XRGB: what the shapes' and premultiplied windows' blending left in the top byte goes
        for y in r.y..r.bottom() {
            for p in &mut dst[y as usize * stride + r.x as usize..][..r.w as usize] {
                *p &= 0x00FF_FFFF;
            }
        }
    }
    pub fn pointer(&self) -> (i32, i32) {
        self.pointer
    }

    /// The surface the pointer is locked to, while the lock is active.
    pub fn pointer_locked(&self) -> Option<(ClientId, u32)> {
        self.locked
    }

    pub fn focus(&self) -> Option<(ClientId, u32)> {
        self.focus
    }

    /// Mapped windows, bottom to top (not the panel).
    pub fn stack(&self) -> &[(ClientId, u32)] {
        &self.stack
    }

    /// A mapped window's frame (title bar included).
    pub fn window_frame(&self, c: ClientId, surface: u32) -> Option<Rect> {
        self.surface((c, surface)).filter(|s| s.mapped).map(|s| s.frame(self.th))
    }

    /// A mapped window's content box on screen.
    pub fn window_content(&self, c: ClientId, surface: u32) -> Option<Rect> {
        self.surface((c, surface)).filter(|s| s.mapped).map(|s| s.content(self.th))
    }

    pub fn window_title(&self, c: ClientId, surface: u32) -> Option<&str> {
        self.surface((c, surface)).map(|s| s.title.as_str())
    }

    pub fn is_fullscreen(&self, c: ClientId, surface: u32) -> bool {
        self.surface((c, surface)).is_some_and(|s| s.fullscreen.is_some())
    }

    pub fn is_maximized(&self, c: ClientId, surface: u32) -> bool {
        self.surface((c, surface)).is_some_and(|s| s.maximized.is_some())
    }

    /// The frame a resize drag's outline shows, while one is under way.
    pub fn resize_outline(&self) -> Option<Rect> {
        self.resize.as_ref().map(|r| r.outline)
    }

    /// The panel's surface, if a client has the role.
    pub fn panel(&self) -> Option<(ClientId, u32)> {
        self.panel.map(|(k, _)| k)
    }

    /// Toplevel ids of the mapped windows, in the order they appeared.
    pub fn toplevels(&self) -> Vec<(u32, (ClientId, u32))> {
        self.toplevels.clone()
    }

    pub fn damage(&self) -> &Region {
        &self.damage
    }
}

fn fill(dst: &mut [u32], stride: usize, r: Rect, color: u32) {
    for py in r.y..r.bottom() {
        dst[py as usize * stride + r.x as usize..][..r.w as usize].fill(color);
    }
}

/// Copies `dmg` (surface coordinates, already clipped to the buffer) from
/// the client's buffer into the surface's store. The buffer was checked
/// against its pool at creation, so every row read is inside the mapping.
fn copy_damage<M: PoolMem>(b: &BufRef<M>, store: &mut [u32], dmg: &Region) {
    let base = b.mem.as_ptr();
    for r in dmg.rects() {
        for y in r.y..r.bottom() {
            let src = b.offset + y as usize * b.stride + r.x as usize * 4;
            let dst = &mut store[(y * b.w + r.x) as usize..][..r.w as usize];
            // The client may be writing this memory right now (it is
            // shared); a byte copy of a torn frame is the worst outcome.
            unsafe {
                core::ptr::copy_nonoverlapping(base.add(src), dst.as_mut_ptr() as *mut u8, r.w as usize * 4);
            }
        }
    }
}

#[cfg(test)]
mod tests;
