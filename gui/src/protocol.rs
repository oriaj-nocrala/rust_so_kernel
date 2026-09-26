//! This compositor's protocol: minimal, with Wayland's names and model.
//!
//! Object 1 is the compositor, present from the start. Every other object
//! is created by the client, which picks its id (`new_id`); destroying one
//! is answered with `delete_id` on object 1 so the client may reuse it.
//!
//! | interface  | requests (opcode)                                                        | events (opcode) |
//! |------------|--------------------------------------------------------------------------|-----------------|
//! | compositor | create_pool(id, fd, size) 0, create_surface(id) 1, sync(id) 2            | error(obj, code, msg) 0, delete_id(id) 1 |
//! | pool       | create_buffer(id, offset, w, h, stride, format) 0, destroy 1             | — |
//! | buffer     | destroy 0                                                                | release 0 |
//! | surface    | attach(buffer) 0, damage(x, y, w, h) 1, frame(id) 2, commit 3, set_title(s) 4, destroy 5, lock_pointer(on) 6 | configure(w, h) 0, focus(in) 1, key(code, state) 2, motion(x, y) 3, button(code, state) 4, relative_motion(dx, dy) 5 |
//! | callback   | —                                                                        | done(ms) 0 |
//!
//! It folds `wl_display`, `wl_compositor`, `wl_shm`, `wl_surface`,
//! `xdg_toplevel` and `wl_seat` into five interfaces; porting libwayland
//! later would split them, not change the model. `lock_pointer` and
//! `relative_motion` are Wayland's pointer-constraints and relative-pointer
//! extensions folded in the same way (see `Compositor`'s pointer lock). One pixel format:
//! `XRGB8888` (value 1, as `wl_shm`'s).

use alloc::string::String;

use crate::wire::{Decoder, Encoder, Message, WireError};

pub const COMPOSITOR_ID: u32 = 1;
/// `wl_shm`'s `XRGB8888`: 32 bits per pixel, `0x00RRGGBB`.
pub const FORMAT_XRGB8888: u32 = 1;
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    CreatePool { id: u32, fd: i32, size: u32 },
    CreateSurface { id: u32 },
    Sync { id: u32 },
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
    Done { callback: u32, ms: u32 },
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
            (Interface::Compositor, 2) => Request::Sync { id: a.uint()? },
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
            (Interface::Callback, 0) => Event::Done { callback: obj, ms: a.uint()? },
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
            Event::Done { callback, ms } => e.begin(*callback, 0).uint(*ms),
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
            | Event::RelativeMotion { surface, .. } => *surface,
            Event::Done { callback, .. } => *callback,
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    fn requests() -> Vec<(Interface, Request)> {
        vec![
            (Interface::Compositor, Request::CreatePool { id: 2, fd: 17, size: 4096 }),
            (Interface::Compositor, Request::CreateSurface { id: 3 }),
            (Interface::Compositor, Request::Sync { id: 9 }),
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
        ]
    }

    #[test]
    fn every_request_roundtrips() {
        let mut e = Encoder::new();
        for (_, r) in requests() {
            r.encode(&mut e);
        }
        let (bytes, fds) = e.take();
        assert_eq!(fds, vec![17]);
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
            (Interface::Callback, Event::Done { callback: 8, ms: 1234 }),
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
        e.begin(3, 9).end(); // surface has no opcode 9
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
