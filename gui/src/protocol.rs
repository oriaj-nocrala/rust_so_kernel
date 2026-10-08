//! This compositor's protocol: minimal, with Wayland's names and model.
//!
//! Object 1 is the compositor, present from the start. Every other object
//! is created by the client, which picks its id (`new_id`); destroying one
//! is answered with `delete_id` on object 1 so the client may reuse it.
//!
//! | interface  | requests (opcode)                                                        | events (opcode) |
//! |------------|--------------------------------------------------------------------------|-----------------|
//! | compositor | create_pool(id, fd, size) 0, create_surface(id) 1, sync(id) 2, create_gpu_buffer(id, fd, size, w, h, stride, format) 3, get_semantics(id) 4 | error(obj, code, msg) 0, delete_id(id) 1 |
//! | pool       | create_buffer(id, offset, w, h, stride, format) 0, destroy 1             | — |
//! | buffer     | destroy 0                                                                | release 0 |
//! | surface    | attach(buffer) 0, damage(x, y, w, h) 1, frame(id) 2, commit 3, set_title(s) 4, destroy 5, lock_pointer(on) 6, set_resizable(min_w, min_h) 7, set_panel(height) 8, activate(toplevel) 9, set_popup(parent, x, y) 10, set_theme(name) 11, semantics_node(node) 12 | configure(w, h) 0, focus(in) 1, key(code, state) 2, motion(x, y) 3, button(code, state) 4, relative_motion(dx, dy) 5, resize(w, h) 6, close 7, toplevel(id, title) 8, toplevel_focus(id) 9, toplevel_gone(id) 10, theme(name) 11, popup_done 12 |
//! | callback   | —                                                                        | done(ms) 0, semantics_window(toplevel, title, x, y, w, h, focused) 1, semantics_node(node) 2 |
//!
//! `create_gpu_buffer` makes a buffer (a `buffer` object: `destroy`, `release`) out of a GPU
//! buffer the client exports as a descriptor (`/dev/nvgpu`'s `BO_EXPORT`): the compositor does not
//! copy it, it reads it on the GPU, so the buffer stays in use until the client's next `commit` has
//! replaced it *and* the compositor's frame that read it is done; only then is `release` sent.
//! The client must have finished writing it (waited for its GPU work) before `commit`.
//!
//! It folds `wl_display`, `wl_compositor`, `wl_shm`, `wl_surface`,
//! `xdg_toplevel` and `wl_seat` into five interfaces; porting libwayland
//! later would split them, not change the model. `lock_pointer` and
//! `relative_motion` are Wayland's pointer-constraints and relative-pointer
//! extensions folded in the same way (see `Compositor`'s pointer lock). Pixel formats, `wl_shm`'s values:
//! `XRGB8888` (1), and for pool buffers `ARGB8888` (0), **premultiplied** as Wayland's is, shown "over" what is under the surface.
//!
//! The semantic tree (`semantic`): `semantics_node`'s `node` is the arguments id, parent, role, flags, actions, x, y, w, h, pos, set_size,
//! name, value (uints, ints, then two strings); the nodes sent before a `commit` replace the surface's tree at it. `get_semantics` makes a
//! callback object and answers on it: for every window (stacking order, bottom first) `semantics_window` with its toplevel id, title,
//! content box on the screen and focus, then that window's nodes; then `done`, and the id is deleted. Any client may ask (as any client
//! may connect: the socket is the boundary today).
//!
//! Window management (phase 4): `set_resizable` is `xdg_toplevel`'s
//! `set_min_size` and the opt-in to `resize` (and to F11 fullscreen), a *request* for a content of
//! `w x h` (the real size stays that of the next committed buffer, so
//! there is no `ack_configure`); `close` is `xdg_toplevel.close`.
//! `set_panel` is a `wlr-layer-shell`-like role — a strip along the bottom,
//! undecorated, above every window, out of the work area — and its surface
//! alone gets the window list (`toplevel*`, with the compositor's own ids)
//! and may `activate` one. It is also told the compositor's look (`theme`: a name of
//! `gui::theme`, at `set_panel` and whenever it changes) so it can draw the taskbar's buttons to
//! match; the strip under it is the compositor's. The panel alone may `set_theme` (its menu's
//! theme selector). `set_popup` is `xdg_popup`: the surface (before it is mapped) becomes a popup
//! at (x, y) of another surface of the same client, shown above everything, undecorated, never
//! focused; a click outside every popup, or Escape, hides it and sends `popup_done` (that click
//! goes nowhere), and so does its parent going away. Clients ignore events they do not know, so an
//! old client never sees a difference.

use alloc::string::String;

use crate::region::Rect;
use crate::semantic::{Node, Role};
use crate::wire::{Args, Decoder, Encoder, Message, WireError};

pub const COMPOSITOR_ID: u32 = 1;
/// `wl_shm`'s `XRGB8888`: 32 bits per pixel, `0x00RRGGBB`.
pub const FORMAT_XRGB8888: u32 = 1;
/// `wl_shm`'s `ARGB8888`, premultiplied: `0xAARRGGBB` with each colour already multiplied by alpha. Pool buffers only.
pub const FORMAT_ARGB8888: u32 = 0;
/// Longest title accepted, in bytes.
pub const MAX_TITLE: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interface {
    Compositor,
    Pool,
    Buffer,
    Surface,
    Callback,
}

/// The `code` of an `error` event. The client is disconnected after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ErrorCode {
    /// A request named an object that does not exist, or of the wrong kind.
    InvalidObject = 0,
    /// An opcode the interface does not have, or malformed arguments.
    InvalidMethod = 1,
    NoMemory = 2,
    /// A `new_id` of 0 or already in use.
    InvalidId = 3,
    InvalidFormat = 4,
    /// A buffer that does not fit in its pool, or a bad stride or size.
    InvalidBuffer = 5,
    /// The pool's memory could not be mapped.
    BadPool = 6,
    /// `set_panel` while another surface has the role, or after the
    /// surface was mapped as a window.
    Role = 7,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    CreatePool { id: u32, fd: i32, size: u32 },
    CreateSurface { id: u32 },
    Sync { id: u32 },
    /// A buffer that lives on the GPU: `fd` is a descriptor of `size` bytes of it, `stride` bytes per row.
    CreateGpuBuffer { id: u32, fd: i32, size: u32, width: i32, height: i32, stride: i32, format: u32 },
    CreateBuffer { pool: u32, id: u32, offset: i32, width: i32, height: i32, stride: i32, format: u32 },
    DestroyPool { pool: u32 },
    DestroyBuffer { buffer: u32 },
    /// `buffer == 0` detaches (the window is unmapped at the next commit).
    Attach { surface: u32, buffer: u32 },
    Damage { surface: u32, x: i32, y: i32, w: i32, h: i32 },
    Frame { surface: u32, id: u32 },
    Commit { surface: u32 },
    SetTitle { surface: u32, title: String },
    DestroySurface { surface: u32 },
    /// Ask for (or give up) the pointer: while locked, motion arrives as
    /// `relative_motion` and the pointer stays put. For games.
    LockPointer { surface: u32, on: bool },
    /// The window may be resized, down to `min_w x min_h` of content; the
    /// client answers `resize` events.
    SetResizable { surface: u32, min_w: i32, min_h: i32 },
    /// The panel role: `height` pixels along the bottom of the screen.
    SetPanel { surface: u32, height: i32 },
    /// From the panel: raise and focus the window with that toplevel id.
    Activate { surface: u32, toplevel: u32 },
    /// The popup role (before the surface is mapped): shown at (`x`, `y`) of `parent`'s content, a surface of the same client.
    SetPopup { surface: u32, parent: u32, x: i32, y: i32 },
    /// From the panel: the compositor's look (`gui::theme`'s names).
    SetTheme { surface: u32, name: String },
    /// One node of the surface's next semantic tree (`semantic`).
    SemanticsNode { surface: u32, node: Node },
    /// Every window's semantic tree, answered on the new callback `id`.
    GetSemantics { id: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Error { object: u32, code: u32, message: String },
    DeleteId { id: u32 },
    Release { buffer: u32 },
    Configure { surface: u32, width: i32, height: i32 },
    Focus { surface: u32, focused: bool },
    /// `code` is a Linux `KEY_*`; `pressed` is 1 on press, 0 on release.
    Key { surface: u32, code: u32, pressed: bool },
    /// Surface-local coordinates.
    Motion { surface: u32, x: i32, y: i32 },
    /// `code` is a Linux `BTN_*`.
    Button { surface: u32, code: u32, pressed: bool },
    /// Pointer motion while locked to this surface; screen convention
    /// (`dy` positive is down).
    RelativeMotion { surface: u32, dx: i32, dy: i32 },
    /// Please draw a content of `width x height` (only after
    /// `set_resizable`).
    Resize { surface: u32, width: i32, height: i32 },
    /// The user asked the window to close; the client decides.
    Close { surface: u32 },
    /// To the panel: a window appeared or changed its title.
    Toplevel { surface: u32, id: u32, title: String },
    /// To the panel: the focused window (0: none).
    ToplevelFocus { surface: u32, id: u32 },
    /// To the panel: the window went away.
    ToplevelGone { surface: u32, id: u32 },
    /// To the panel: the compositor's look (`gui::theme`'s names: "flat", "luna", "9x").
    Theme { surface: u32, name: String },
    /// The popup was dismissed (a click outside it, Escape, its parent gone) and is hidden; it stays a popup and shows again at its
    /// next commit with a buffer.
    PopupDone { surface: u32 },
    Done { callback: u32, ms: u32 },
    /// Answering `get_semantics`: a window; its nodes follow. `x, y, w, h`: its content on the screen.
    SemanticsWindow { callback: u32, toplevel: u32, title: String, x: i32, y: i32, w: i32, h: i32, focused: bool },
    /// Answering `get_semantics`: a node of the last window announced.
    SemanticsNode { callback: u32, node: Node },
}

fn encode_node(e: &mut Encoder, n: &Node) {
    let b = n.bounds;
    e.uint(n.id).uint(n.parent).uint(n.role as u32).uint(n.flags).uint(n.actions);
    e.int(b.x).int(b.y).int(b.w).int(b.h).uint(n.pos).uint(n.set_size).string(&n.name).string(&n.value);
}

fn decode_node(a: &mut Args) -> Result<Node, WireError> {
    let (id, parent, role, flags, actions) = (a.uint()?, a.uint()?, Role::from_u32(a.uint()?), a.uint()?, a.uint()?);
    let bounds = Rect::new(a.int()?, a.int()?, a.int()?, a.int()?);
    let (pos, set_size) = (a.uint()?, a.uint()?);
    let (name, value) = (a.string()?, a.string()?);
    Ok(Node { id, parent, role, flags, actions, bounds, pos, set_size, name, value })
}

/// Why a message could not be turned into a request or event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Wire(WireError),
    UnknownOpcode,
}

impl From<WireError> for DecodeError {
    fn from(e: WireError) -> Self {
        DecodeError::Wire(e)
    }
}

impl Request {
    /// Decodes `msg`, sent to an object of interface `iface`. An `fd`
    /// argument takes the next fd from `fds`.
    pub fn decode(iface: Interface, msg: &Message, fds: &mut Decoder) -> Result<Request, DecodeError> {
        let obj = msg.object;
        let mut a = msg.args();
        let r = match (iface, msg.opcode) {
            (Interface::Compositor, 0) => {
                let id = a.uint()?;
                let size = a.uint()?;
                a.finish()?;
                let fd = fds.take_fd()?;
                Request::CreatePool { id, fd, size }
            }
            (Interface::Compositor, 1) => Request::CreateSurface { id: a.uint()? },
            (Interface::Compositor, 4) => Request::GetSemantics { id: a.uint()? },
            (Interface::Compositor, 2) => Request::Sync { id: a.uint()? },
            (Interface::Compositor, 3) => {
                let (id, size) = (a.uint()?, a.uint()?);
                let (width, height, stride, format) = (a.int()?, a.int()?, a.int()?, a.uint()?);
                a.finish()?;
                let fd = fds.take_fd()?;
                Request::CreateGpuBuffer { id, fd, size, width, height, stride, format }
            }
            (Interface::Pool, 0) => Request::CreateBuffer {
                pool: obj,
                id: a.uint()?,
                offset: a.int()?,
                width: a.int()?,
                height: a.int()?,
                stride: a.int()?,
                format: a.uint()?,
            },
            (Interface::Pool, 1) => Request::DestroyPool { pool: obj },
            (Interface::Buffer, 0) => Request::DestroyBuffer { buffer: obj },
            (Interface::Surface, 0) => Request::Attach { surface: obj, buffer: a.uint()? },
            (Interface::Surface, 1) => Request::Damage { surface: obj, x: a.int()?, y: a.int()?, w: a.int()?, h: a.int()? },
            (Interface::Surface, 2) => Request::Frame { surface: obj, id: a.uint()? },
            (Interface::Surface, 3) => Request::Commit { surface: obj },
            (Interface::Surface, 4) => Request::SetTitle { surface: obj, title: a.string()? },
            (Interface::Surface, 5) => Request::DestroySurface { surface: obj },
            (Interface::Surface, 6) => Request::LockPointer { surface: obj, on: a.uint()? != 0 },
            (Interface::Surface, 7) => Request::SetResizable { surface: obj, min_w: a.int()?, min_h: a.int()? },
            (Interface::Surface, 8) => Request::SetPanel { surface: obj, height: a.int()? },
            (Interface::Surface, 9) => Request::Activate { surface: obj, toplevel: a.uint()? },
            (Interface::Surface, 10) => Request::SetPopup { surface: obj, parent: a.uint()?, x: a.int()?, y: a.int()? },
            (Interface::Surface, 11) => Request::SetTheme { surface: obj, name: a.string()? },
            (Interface::Surface, 12) => Request::SemanticsNode { surface: obj, node: decode_node(&mut a)? },
            _ => return Err(DecodeError::UnknownOpcode),
        };
        a.finish()?;
        Ok(r)
    }

    pub fn encode(&self, e: &mut Encoder) {
        match self {
            Request::CreatePool { id, fd, size } => e.begin(COMPOSITOR_ID, 0).uint(*id).fd(*fd).uint(*size),
            Request::CreateSurface { id } => e.begin(COMPOSITOR_ID, 1).uint(*id),
            Request::Sync { id } => e.begin(COMPOSITOR_ID, 2).uint(*id),
            Request::CreateGpuBuffer { id, fd, size, width, height, stride, format } => {
                e.begin(COMPOSITOR_ID, 3).uint(*id).fd(*fd).uint(*size).int(*width).int(*height).int(*stride).uint(*format)
            }
            Request::CreateBuffer { pool, id, offset, width, height, stride, format } => e
                .begin(*pool, 0)
                .uint(*id)
                .int(*offset)
                .int(*width)
                .int(*height)
                .int(*stride)
                .uint(*format),
            Request::DestroyPool { pool } => e.begin(*pool, 1),
            Request::DestroyBuffer { buffer } => e.begin(*buffer, 0),
            Request::Attach { surface, buffer } => e.begin(*surface, 0).uint(*buffer),
            Request::Damage { surface, x, y, w, h } => e.begin(*surface, 1).int(*x).int(*y).int(*w).int(*h),
            Request::Frame { surface, id } => e.begin(*surface, 2).uint(*id),
            Request::Commit { surface } => e.begin(*surface, 3),
            Request::SetTitle { surface, title } => e.begin(*surface, 4).string(title),
            Request::DestroySurface { surface } => e.begin(*surface, 5),
            Request::LockPointer { surface, on } => e.begin(*surface, 6).uint(*on as u32),
            Request::SetResizable { surface, min_w, min_h } => e.begin(*surface, 7).int(*min_w).int(*min_h),
            Request::SetPanel { surface, height } => e.begin(*surface, 8).int(*height),
            Request::Activate { surface, toplevel } => e.begin(*surface, 9).uint(*toplevel),
            Request::SetPopup { surface, parent, x, y } => e.begin(*surface, 10).uint(*parent).int(*x).int(*y),
            Request::SetTheme { surface, name } => e.begin(*surface, 11).string(name),
            Request::GetSemantics { id } => e.begin(COMPOSITOR_ID, 4).uint(*id),
            Request::SemanticsNode { surface, node } => {
                e.begin(*surface, 12);
                encode_node(e, node);
                e
            }
        }
        .end();
    }
}

impl Event {
    pub fn decode(iface: Interface, msg: &Message) -> Result<Event, DecodeError> {
        let obj = msg.object;
        let mut a = msg.args();
        let ev = match (iface, msg.opcode) {
            (Interface::Compositor, 0) => Event::Error { object: a.uint()?, code: a.uint()?, message: a.string()? },
            (Interface::Compositor, 1) => Event::DeleteId { id: a.uint()? },
            (Interface::Buffer, 0) => Event::Release { buffer: obj },
            (Interface::Surface, 0) => Event::Configure { surface: obj, width: a.int()?, height: a.int()? },
            (Interface::Surface, 1) => Event::Focus { surface: obj, focused: a.uint()? != 0 },
            (Interface::Surface, 2) => Event::Key { surface: obj, code: a.uint()?, pressed: a.uint()? != 0 },
            (Interface::Surface, 3) => Event::Motion { surface: obj, x: a.int()?, y: a.int()? },
            (Interface::Surface, 4) => Event::Button { surface: obj, code: a.uint()?, pressed: a.uint()? != 0 },
            (Interface::Surface, 5) => Event::RelativeMotion { surface: obj, dx: a.int()?, dy: a.int()? },
            (Interface::Surface, 6) => Event::Resize { surface: obj, width: a.int()?, height: a.int()? },
            (Interface::Surface, 7) => Event::Close { surface: obj },
            (Interface::Surface, 8) => Event::Toplevel { surface: obj, id: a.uint()?, title: a.string()? },
            (Interface::Surface, 9) => Event::ToplevelFocus { surface: obj, id: a.uint()? },
            (Interface::Surface, 10) => Event::ToplevelGone { surface: obj, id: a.uint()? },
            (Interface::Surface, 11) => Event::Theme { surface: obj, name: a.string()? },
            (Interface::Surface, 12) => Event::PopupDone { surface: obj },
            (Interface::Callback, 0) => Event::Done { callback: obj, ms: a.uint()? },
            (Interface::Callback, 1) => Event::SemanticsWindow {
                callback: obj,
                toplevel: a.uint()?,
                title: a.string()?,
                x: a.int()?,
                y: a.int()?,
                w: a.int()?,
                h: a.int()?,
                focused: a.uint()? != 0,
            },
            (Interface::Callback, 2) => Event::SemanticsNode { callback: obj, node: decode_node(&mut a)? },
            _ => return Err(DecodeError::UnknownOpcode),
        };
        a.finish()?;
        Ok(ev)
    }

    pub fn encode(&self, e: &mut Encoder) {
        match self {
            Event::Error { object, code, message } => e.begin(COMPOSITOR_ID, 0).uint(*object).uint(*code).string(message),
            Event::DeleteId { id } => e.begin(COMPOSITOR_ID, 1).uint(*id),
            Event::Release { buffer } => e.begin(*buffer, 0),
            Event::Configure { surface, width, height } => e.begin(*surface, 0).int(*width).int(*height),
            Event::Focus { surface, focused } => e.begin(*surface, 1).uint(*focused as u32),
            Event::Key { surface, code, pressed } => e.begin(*surface, 2).uint(*code).uint(*pressed as u32),
            Event::Motion { surface, x, y } => e.begin(*surface, 3).int(*x).int(*y),
            Event::Button { surface, code, pressed } => e.begin(*surface, 4).uint(*code).uint(*pressed as u32),
            Event::RelativeMotion { surface, dx, dy } => e.begin(*surface, 5).int(*dx).int(*dy),
            Event::Resize { surface, width, height } => e.begin(*surface, 6).int(*width).int(*height),
            Event::Close { surface } => e.begin(*surface, 7),
            Event::Toplevel { surface, id, title } => e.begin(*surface, 8).uint(*id).string(title),
            Event::ToplevelFocus { surface, id } => e.begin(*surface, 9).uint(*id),
            Event::ToplevelGone { surface, id } => e.begin(*surface, 10).uint(*id),
            Event::Theme { surface, name } => e.begin(*surface, 11).string(name),
            Event::PopupDone { surface } => e.begin(*surface, 12),
            Event::Done { callback, ms } => e.begin(*callback, 0).uint(*ms),
            Event::SemanticsWindow { callback, toplevel, title, x, y, w, h, focused } => {
                e.begin(*callback, 1).uint(*toplevel).string(title).int(*x).int(*y).int(*w).int(*h).uint(*focused as u32)
            }
            Event::SemanticsNode { callback, node } => {
                e.begin(*callback, 2);
                encode_node(e, node);
                e
            }
        }
        .end();
    }

    /// The object the event is addressed to.
    pub fn object(&self) -> u32 {
        match self {
            Event::Error { .. } | Event::DeleteId { .. } => COMPOSITOR_ID,
            Event::Release { buffer } => *buffer,
            Event::Configure { surface, .. }
            | Event::Focus { surface, .. }
            | Event::Key { surface, .. }
            | Event::Motion { surface, .. }
            | Event::Button { surface, .. }
            | Event::RelativeMotion { surface, .. }
            | Event::Resize { surface, .. }
            | Event::Close { surface }
            | Event::Toplevel { surface, .. }
            | Event::ToplevelFocus { surface, .. }
            | Event::ToplevelGone { surface, .. }
            | Event::Theme { surface, .. }
            | Event::PopupDone { surface } => *surface,
            Event::Done { callback, .. } | Event::SemanticsWindow { callback, .. } | Event::SemanticsNode { callback, .. } => *callback,
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    fn sample_node() -> Node {
        let mut n = Node::new(0xdead_beef, 7, Role::ListBoxOption, Rect::new(-3, 20, 300, 22));
        n.flags = crate::semantic::flag::SELECTED;
        n.actions = crate::semantic::action::CLICK | crate::semantic::action::FOCUS;
        n.pos = 9_999;
        n.set_size = 10_000;
        n.name = "reporte final.png".into();
        n.value = "12 KiB".into();
        n
    }

    fn requests() -> Vec<(Interface, Request)> {
        vec![
            (Interface::Compositor, Request::CreatePool { id: 2, fd: 17, size: 4096 }),
            (Interface::Compositor, Request::CreateSurface { id: 3 }),
            (Interface::Compositor, Request::Sync { id: 9 }),
            (Interface::Compositor, Request::CreateGpuBuffer { id: 5, fd: 21, size: 8_294_400, width: 1920, height: 1080, stride: 7680, format: 1 }),
            (Interface::Pool, Request::CreateBuffer { pool: 2, id: 4, offset: 0, width: 10, height: 20, stride: 40, format: 1 }),
            (Interface::Pool, Request::DestroyPool { pool: 2 }),
            (Interface::Buffer, Request::DestroyBuffer { buffer: 4 }),
            (Interface::Surface, Request::Attach { surface: 3, buffer: 0 }),
            (Interface::Surface, Request::Damage { surface: 3, x: -1, y: 2, w: 3, h: 4 }),
            (Interface::Surface, Request::Frame { surface: 3, id: 8 }),
            (Interface::Surface, Request::Commit { surface: 3 }),
            (Interface::Surface, Request::SetTitle { surface: 3, title: "ventana".into() }),
            (Interface::Surface, Request::DestroySurface { surface: 3 }),
            (Interface::Surface, Request::LockPointer { surface: 3, on: true }),
            (Interface::Surface, Request::LockPointer { surface: 3, on: false }),
            (Interface::Surface, Request::SetResizable { surface: 3, min_w: 100, min_h: 50 }),
            (Interface::Surface, Request::SetPanel { surface: 3, height: 32 }),
            (Interface::Surface, Request::Activate { surface: 3, toplevel: 7 }),
            (Interface::Surface, Request::SetPopup { surface: 3, parent: 4, x: -2, y: -300 }),
            (Interface::Surface, Request::SetTheme { surface: 3, name: "9x".into() }),
            (Interface::Compositor, Request::GetSemantics { id: 12 }),
            (Interface::Surface, Request::SemanticsNode { surface: 3, node: sample_node() }),
        ]
    }

    #[test]
    fn every_request_roundtrips() {
        let mut e = Encoder::new();
        for (_, r) in requests() {
            r.encode(&mut e);
        }
        let (bytes, fds) = e.take();
        assert_eq!(fds, vec![17, 21]);
        let mut d = Decoder::new();
        d.push_bytes(&bytes);
        d.push_fds(&fds);
        for (iface, want) in requests() {
            let m = d.next_message().unwrap().unwrap();
            assert_eq!(Request::decode(iface, &m, &mut d).unwrap(), want);
        }
        assert!(d.next_message().unwrap().is_none());
    }

    #[test]
    fn every_event_roundtrips() {
        let evs = vec![
            (Interface::Compositor, Event::Error { object: 5, code: 3, message: "bad id".into() }),
            (Interface::Compositor, Event::DeleteId { id: 5 }),
            (Interface::Buffer, Event::Release { buffer: 4 }),
            (Interface::Surface, Event::Configure { surface: 3, width: 640, height: 480 }),
            (Interface::Surface, Event::Focus { surface: 3, focused: true }),
            (Interface::Surface, Event::Key { surface: 3, code: 30, pressed: false }),
            (Interface::Surface, Event::Motion { surface: 3, x: -2, y: 7 }),
            (Interface::Surface, Event::Button { surface: 3, code: 0x110, pressed: true }),
            (Interface::Surface, Event::RelativeMotion { surface: 3, dx: -5, dy: 12 }),
            (Interface::Surface, Event::Resize { surface: 3, width: 800, height: 600 }),
            (Interface::Surface, Event::Close { surface: 3 }),
            (Interface::Surface, Event::Toplevel { surface: 3, id: 2, title: "term".into() }),
            (Interface::Surface, Event::ToplevelFocus { surface: 3, id: 0 }),
            (Interface::Surface, Event::ToplevelGone { surface: 3, id: 2 }),
            (Interface::Surface, Event::Theme { surface: 3, name: "luna".into() }),
            (Interface::Surface, Event::PopupDone { surface: 3 }),
            (Interface::Callback, Event::Done { callback: 8, ms: 1234 }),
            (
                Interface::Callback,
                Event::SemanticsWindow { callback: 8, toplevel: 2, title: "Files".into(), x: 40, y: 60, w: 320, h: -1, focused: true },
            ),
            (Interface::Callback, Event::SemanticsNode { callback: 8, node: sample_node() }),
        ];
        let mut e = Encoder::new();
        for (_, ev) in &evs {
            ev.encode(&mut e);
        }
        let (bytes, fds) = e.take();
        assert!(fds.is_empty());
        let mut d = Decoder::new();
        d.push_bytes(&bytes);
        for (iface, want) in evs {
            let m = d.next_message().unwrap().unwrap();
            assert_eq!(m.object, want.object());
            assert_eq!(Event::decode(iface, &m).unwrap(), want);
        }
    }

    #[test]
    fn wrong_opcode_or_arguments() {
        let mut e = Encoder::new();
        e.begin(3, 99).end(); // surface has no opcode 99
        e.begin(3, 3).uint(1).end(); // commit takes no arguments
        e.begin(1, 0).uint(2).uint(4096).end(); // create_pool without its fd
        let (bytes, _) = e.take();
        let mut d = Decoder::new();
        d.push_bytes(&bytes);
        let m = d.next_message().unwrap().unwrap();
        assert_eq!(Request::decode(Interface::Surface, &m, &mut d), Err(DecodeError::UnknownOpcode));
        let m = d.next_message().unwrap().unwrap();
        assert_eq!(Request::decode(Interface::Surface, &m, &mut d), Err(DecodeError::Wire(WireError::TrailingBytes)));
        let m = d.next_message().unwrap().unwrap();
        assert_eq!(Request::decode(Interface::Compositor, &m, &mut d), Err(DecodeError::Wire(WireError::MissingFd)));
    }
}
