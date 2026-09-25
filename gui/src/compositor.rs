//! The compositor's state: clients, their objects, surfaces, stacking,
//! focus, the pointer, and [`Compositor::compose`].
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
//! **A buffer that does not fit its pool is a protocol error**, checked at
//! `create_buffer` against the pool's size — never a read out of bounds.
//! (A pool cannot shrink under us: `ftruncate` of a mapped memfd is
//! `EBUSY` in this kernel.) Every protocol error sends `error` and
//! disconnects the client, as Wayland does.

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use crate::protocol::{DecodeError, ErrorCode, Event, Interface, Request, FORMAT_XRGB8888, MAX_TITLE};
use crate::region::{Rect, Region};
use crate::wire::{Decoder, WireError};

pub type ClientId = u32;

/// A pool's memory as mapped by the host. Real ones are `mmap`s of a memfd
/// (unmapped on drop); tests hand out a `Vec`'s pointer.
pub trait PoolMem {
    fn as_ptr(&self) -> *const u8;
    fn len(&self) -> usize;
}

pub const TITLE_H: i32 = 20;
pub const BACKGROUND: u32 = 0x0020_3040;
pub const TITLE_FOCUSED: u32 = 0x0050_78B0;
pub const TITLE_UNFOCUSED: u32 = 0x0050_5058;
/// Largest surface side accepted.
pub const MAX_SIDE: i32 = 8192;
pub const MAX_OBJECTS: usize = 512;
/// Most rectangles `compose` reports (`FBIO_FLUSH` takes 16 per call).
pub const MAX_FLUSH_RECTS: usize = 16;

pub const BTN_LEFT: u32 = 0x110;
const KEY_BACKSPACE: u32 = 14;
const KEY_LEFTCTRL: u32 = 29;
const KEY_RIGHTCTRL: u32 = 97;
const KEY_LEFTALT: u32 = 56;
const KEY_RIGHTALT: u32 = 100;

/// The software cursor: `X` black, `.` white, space transparent. Hotspot
/// at its top-left corner.
const CURSOR: [&[u8; 11]; 16] = [
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

/// A buffer: a window into a pool. The pool's memory lives as long as any
/// buffer of it, even after the pool is destroyed (Wayland's rule).
struct BufRef<M> {
    id: u32,
    mem: Rc<M>,
    offset: usize,
    w: i32,
    h: i32,
    stride: usize,
}

impl<M> Clone for BufRef<M> {
    fn clone(&self) -> Self {
        BufRef { id: self.id, mem: self.mem.clone(), offset: self.offset, w: self.w, h: self.h, stride: self.stride }
    }
}

struct Surface<M> {
    /// `None`: nothing attached since the last commit. `Some(None)`: a null
    /// attach (unmap at commit).
    pending_buffer: Option<Option<BufRef<M>>>,
    pending_damage: Region,
    pending_frames: Vec<u32>,
    title: String,
    /// Current content, `w x h`, row-major.
    store: Vec<u32>,
    w: i32,
    h: i32,
    mapped: bool,
    /// Top-left of the window's frame (title bar included).
    x: i32,
    y: i32,
}

impl<M> Surface<M> {
    fn frame(&self) -> Rect {
        Rect::new(self.x, self.y, self.w, self.h + TITLE_H)
    }
    fn content(&self) -> Rect {
        Rect::new(self.x, self.y + TITLE_H, self.w, self.h)
    }
    fn title_bar(&self) -> Rect {
        Rect::new(self.x, self.y, self.w, TITLE_H)
    }
}

enum Object<M> {
    Pool { mem: Rc<M>, size: usize },
    Buffer(BufRef<M>),
    Surface(Surface<M>),
    Callback,
}

impl<M> Object<M> {
    fn interface(&self) -> Interface {
        match self {
            Object::Pool { .. } => Interface::Pool,
            Object::Buffer(_) => Interface::Buffer,
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

pub struct Compositor<M> {
    width: i32,
    height: i32,
    clients: BTreeMap<ClientId, Client<M>>,
    next_client: ClientId,
    /// Mapped surfaces, bottom to top.
    stack: Vec<Key>,
    focus: Option<Key>,
    pointer: (i32, i32),
    drag: Option<Drag>,
    /// Where a content-area press went, so its release goes there too.
    button_target: Option<Key>,
    ctrl: u8,
    alt: u8,
    quit: bool,
    placed: i32,
    damage: Region,
    frame_waiting: Vec<(ClientId, u32)>,
    events: Vec<(ClientId, Event)>,
    disconnects: Vec<ClientId>,
    fds_to_close: Vec<i32>,
}

impl<M: PoolMem> Compositor<M> {
    pub fn new(width: i32, height: i32) -> Self {
        Compositor {
            width,
            height,
            clients: BTreeMap::new(),
            next_client: 1,
            stack: Vec::new(),
            focus: None,
            pointer: (width / 2, height / 2),
            drag: None,
            button_target: None,
            ctrl: 0,
            alt: 0,
            quit: false,
            placed: 0,
            // The first compose paints everything.
            damage: Region::from_rect(Rect::new(0, 0, width, height)),
            frame_waiting: Vec::new(),
            events: Vec::new(),
            disconnects: Vec::new(),
            fds_to_close: Vec::new(),
        }
    }

    fn screen(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
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
        for (id, obj) in &client.objects {
            if let Object::Surface(s) = obj {
                if s.mapped {
                    self.damage.add(s.frame());
                }
                self.forget_surface((c, *id));
            }
        }
        self.frame_waiting.retain(|(fc, _)| *fc != c);
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
                    pending_damage: Region::new(),
                    pending_frames: Vec::new(),
                    title: String::new(),
                    store: Vec::new(),
                    w: 0,
                    h: 0,
                    mapped: false,
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
            Request::CreateBuffer { pool, id, offset, width, height, stride, format } => {
                let Some(Object::Pool { mem, size }) = self.clients[&c].objects.get(&pool) else { unreachable!() };
                let (mem, size) = (mem.clone(), *size);
                if format != FORMAT_XRGB8888 {
                    return self.fail(c, pool, ErrorCode::InvalidFormat, "only XRGB8888");
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
                let b = BufRef { id, mem, offset: offset as usize, w: width, h: height, stride: stride as usize };
                self.new_object(c, id, Object::Buffer(b));
            }
            Request::DestroyPool { pool } | Request::DestroyBuffer { buffer: pool } => self.destroy_object(c, pool),
            Request::Attach { surface, buffer } => {
                let b = if buffer == 0 {
                    None
                } else {
                    match self.clients[&c].objects.get(&buffer) {
                        Some(Object::Buffer(b)) => Some(b.clone()),
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
                self.surface_mut((c, surface)).unwrap().title = t;
            }
            Request::DestroySurface { surface } => {
                let key = (c, surface);
                if let Some(s) = self.surface(key) {
                    if s.mapped {
                        self.damage.add(s.frame());
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
        let (sw, sh) = (self.width, self.height);
        let s = self.surface_mut(key).unwrap();
        let frames = core::mem::take(&mut s.pending_frames);
        let damage = core::mem::take(&mut s.pending_damage);
        let mut screen_damage = Region::new();
        let mut released = None;
        let mut newly_mapped = false;
        let mut unmapped = false;
        match s.pending_buffer.take() {
            None => {}
            Some(None) => {
                if s.mapped {
                    screen_damage.add(s.frame());
                    s.mapped = false;
                    unmapped = true;
                }
            }
            Some(Some(b)) => {
                let mut dmg = damage;
                if b.w != s.w || b.h != s.h {
                    if s.mapped {
                        screen_damage.add(s.frame()); // the old size
                    }
                    s.w = b.w;
                    s.h = b.h;
                    s.store = alloc::vec![0; (b.w * b.h) as usize];
                    dmg = Region::from_rect(Rect::new(0, 0, b.w, b.h));
                }
                if !s.mapped {
                    dmg = Region::from_rect(Rect::new(0, 0, b.w, b.h));
                    // Cascade from the top-left, kept on screen.
                    let step = 32 * (placed % 8);
                    s.x = (40 + step).min((sw - b.w).max(0));
                    s.y = (40 + step).min((sh - b.h - TITLE_H).max(0));
                    s.mapped = true;
                    newly_mapped = true;
                }
                dmg.intersect(Rect::new(0, 0, b.w, b.h));
                copy_damage(&b, &mut s.store, &dmg);
                let mut on_screen = dmg.clone();
                on_screen.translate(s.x, s.y + TITLE_H);
                screen_damage.add_region(&on_screen);
                if newly_mapped {
                    screen_damage.add(s.title_bar());
                }
                released = Some(b.id);
            }
        }
        self.damage.add_region(&screen_damage);
        if let Some(id) = released {
            self.events.push((c, Event::Release { buffer: id }));
        }
        self.frame_waiting.extend(frames.into_iter().map(|id| (c, id)));
        if newly_mapped {
            self.placed += 1;
            self.stack.push(key);
            self.set_focus(Some(key));
        }
        if unmapped {
            self.forget_surface(key);
        }
    }

    /// Takes `key` out of the stack and of every input role it had.
    fn forget_surface(&mut self, key: Key) {
        self.stack.retain(|k| *k != key);
        if self.drag.as_ref().is_some_and(|d| d.key == key) {
            self.drag = None;
        }
        if self.button_target == Some(key) {
            self.button_target = None;
        }
        if self.focus == Some(key) {
            self.focus = None;
            let top = self.stack.last().copied();
            self.set_focus(top);
        }
    }

    fn set_focus(&mut self, key: Option<Key>) {
        if self.focus == key {
            return;
        }
        if let Some(old) = self.focus {
            if let Some(s) = self.surface(old) {
                self.damage.add(s.title_bar());
                self.events.push((old.0, Event::Focus { surface: old.1, focused: false }));
            }
        }
        self.focus = key;
        if let Some(new) = key {
            if let Some(s) = self.surface(new) {
                self.damage.add(s.title_bar());
                self.events.push((new.0, Event::Focus { surface: new.1, focused: true }));
            }
        }
    }

    fn raise(&mut self, key: Key) {
        if self.stack.last() != Some(&key) {
            self.stack.retain(|k| *k != key);
            self.stack.push(key);
            if let Some(s) = self.surface(key) {
                self.damage.add(s.frame());
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

    // ── input ─────────────────────────────────────────────────────────────

    fn cursor_rect(&self) -> Rect {
        Rect::new(self.pointer.0, self.pointer.1, CURSOR_W, CURSOR_H)
    }

    /// Topmost mapped surface whose frame holds the point.
    fn window_at(&self, x: i32, y: i32) -> Option<Key> {
        self.stack.iter().rev().copied().find(|k| self.surface(*k).is_some_and(|s| s.frame().contains(x, y)))
    }

    pub fn pointer_motion(&mut self, dx: i32, dy: i32) {
        let old = self.cursor_rect();
        let x = (self.pointer.0 + dx).clamp(0, self.width - 1);
        let y = (self.pointer.1 + dy).clamp(0, self.height - 1);
        if (x, y) == self.pointer {
            return;
        }
        self.pointer = (x, y);
        self.damage.add(old);
        self.damage.add(self.cursor_rect());

        if let Some(d) = &self.drag {
            let (key, nx, ny) = (d.key, x - d.dx, y - d.dy);
            let s = self.surface_mut(key).unwrap();
            let before = s.frame();
            s.x = nx;
            s.y = ny;
            let after = s.frame();
            self.damage.add(before);
            self.damage.add(after);
            return;
        }
        let target = self.button_target.or_else(|| {
            self.window_at(x, y).filter(|k| self.surface(*k).is_some_and(|s| s.content().contains(x, y)))
        });
        if let Some(k) = target {
            let s = self.surface(k).unwrap();
            let (lx, ly) = (x - s.x, y - s.y - TITLE_H);
            self.events.push((k.0, Event::Motion { surface: k.1, x: lx, y: ly }));
        }
    }

    pub fn pointer_button(&mut self, code: u32, pressed: bool) {
        let (x, y) = self.pointer;
        if !pressed {
            if code == BTN_LEFT && self.drag.take().is_some() {
                return;
            }
            if let Some(k) = self.button_target.take() {
                self.events.push((k.0, Event::Button { surface: k.1, code, pressed: false }));
            }
            return;
        }
        let Some(k) = self.window_at(x, y) else { return };
        self.raise(k);
        self.set_focus(Some(k));
        let s = self.surface(k).unwrap();
        if s.title_bar().contains(x, y) {
            if code == BTN_LEFT {
                self.drag = Some(Drag { key: k, dx: x - s.x, dy: y - s.y });
            }
        } else {
            self.button_target = Some(k);
            self.events.push((k.0, Event::Button { surface: k.1, code, pressed: true }));
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

    /// Repaints everything damaged into `dst` (`stride` pixels per row, at
    /// least `width x height`) and returns the rectangles to flush — at
    /// most [`MAX_FLUSH_RECTS`], coarsened to a bounding box beyond that.
    pub fn compose(&mut self, dst: &mut [u32], stride: usize) -> Vec<Rect> {
        self.damage.intersect(self.screen());
        if self.damage.is_empty() || stride < self.width as usize || dst.len() < stride * self.height as usize {
            return Vec::new();
        }
        for r in self.damage.rects().to_vec() {
            self.paint(r, dst, stride);
        }
        let out = self.damage.coarsened(MAX_FLUSH_RECTS);
        self.damage.clear();
        out
    }

    fn paint(&self, r: Rect, dst: &mut [u32], stride: usize) {
        fill(dst, stride, r, BACKGROUND);
        for key in &self.stack {
            let s = self.surface(*key).unwrap();
            if let Some(t) = s.title_bar().intersect(&r) {
                fill(dst, stride, t, if self.focus == Some(*key) { TITLE_FOCUSED } else { TITLE_UNFOCUSED });
            }
            if let Some(i) = s.content().intersect(&r) {
                for py in i.y..i.bottom() {
                    let sy = (py - s.y - TITLE_H) as usize;
                    let sx = (i.x - s.x) as usize;
                    let src = &s.store[sy * s.w as usize + sx..][..i.w as usize];
                    dst[py as usize * stride + i.x as usize..][..i.w as usize].copy_from_slice(src);
                }
            }
        }
        if let Some(i) = self.cursor_rect().intersect(&r) {
            for py in i.y..i.bottom() {
                let row = CURSOR[(py - self.pointer.1) as usize];
                for px in i.x..i.right() {
                    let v = match row[(px - self.pointer.0) as usize] {
                        b'X' => 0x0000_0000,
                        b'.' => 0x00FF_FFFF,
                        _ => continue,
                    };
                    dst[py as usize * stride + px as usize] = v;
                }
            }
        }
    }

    // ── inspection (tests, and the host's own reporting) ─────────────────

    pub fn pointer(&self) -> (i32, i32) {
        self.pointer
    }

    pub fn focus(&self) -> Option<(ClientId, u32)> {
        self.focus
    }

    /// Mapped windows, bottom to top.
    pub fn stack(&self) -> &[(ClientId, u32)] {
        &self.stack
    }

    /// A mapped window's frame (title bar included).
    pub fn window_frame(&self, c: ClientId, surface: u32) -> Option<Rect> {
        self.surface((c, surface)).filter(|s| s.mapped).map(Surface::frame)
    }

    pub fn window_title(&self, c: ClientId, surface: u32) -> Option<&str> {
        self.surface((c, surface)).map(|s| s.title.as_str())
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
