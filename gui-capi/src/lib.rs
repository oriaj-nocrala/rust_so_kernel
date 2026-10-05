//! `gui`'s window manager behind a C ABI (`include/gui_capi.h`). The program on the other side is C (`probes/nvk/vk_comp.c`, linked with NVK
//! on musl); this is a thin skin: handles, plain structs, events pre-encoded to the wire format. No std, one allocator (the C library's).

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int, c_void};

use gui::compositor::{ClientId, Compositor, DrawOp, GpuOp, PoolMem, CURSOR};
use gui::wire::Encoder;

// ---- the C library: allocation and mapping ------------------------------------------------------------------------------------------

extern "C" {
    #[cfg_attr(feature = "std", allow(dead_code))]
    fn malloc(size: usize) -> *mut c_void;
    #[cfg_attr(feature = "std", allow(dead_code))]
    fn free(p: *mut c_void);
    #[cfg_attr(feature = "std", allow(dead_code))]
    fn posix_memalign(out: *mut *mut c_void, align: usize, size: usize) -> c_int;
    fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
    fn fstat(fd: c_int, st: *mut Stat) -> c_int;
    #[cfg_attr(feature = "std", allow(dead_code))]
    fn abort() -> !;
}

const PROT_READ: c_int = 1;
const MAP_SHARED: c_int = 1;

/// The head of `struct stat` on x86-64 Linux/musl: `st_size` is at offset 48.
#[repr(C)]
struct Stat {
    _head: [u8; 48],
    st_size: i64,
    _tail: [u8; 88],
}

#[cfg_attr(feature = "std", allow(dead_code))]
struct CAlloc;

#[cfg(not(feature = "std"))]
unsafe impl core::alloc::GlobalAlloc for CAlloc {
    unsafe fn alloc(&self, l: core::alloc::Layout) -> *mut u8 {
        if l.align() <= 16 {
            malloc(l.size()) as *mut u8
        } else {
            let mut p: *mut c_void = core::ptr::null_mut();
            if posix_memalign(&mut p, l.align(), l.size()) != 0 {
                return core::ptr::null_mut();
            }
            p as *mut u8
        }
    }
    unsafe fn dealloc(&self, p: *mut u8, _l: core::alloc::Layout) {
        free(p as *mut c_void)
    }
}

#[cfg(not(feature = "std"))]
#[global_allocator]
static ALLOC: CAlloc = CAlloc;

#[cfg(not(feature = "std"))]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    unsafe { abort() }
}

/// A client's pool, mapped shared; unmapped when the last buffer of it goes.
struct Mapping {
    addr: *mut c_void,
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
        unsafe { munmap(self.addr, self.len) };
    }
}

/// Maps a pool the client says is `size` bytes, after checking that the file really is that big (a page past its end would fault this
/// process, not the client).
fn map_pool(fd: i32, size: usize) -> Option<Mapping> {
    let mut st = Stat { _head: [0; 48], st_size: 0, _tail: [0; 88] };
    if size == 0 || unsafe { fstat(fd, &mut st) } != 0 || (st.st_size as u64) < size as u64 {
        return None;
    }
    let len = (size + 4095) & !4095;
    let a = unsafe { mmap(core::ptr::null_mut(), len, PROT_READ, MAP_SHARED, fd, 0) };
    if a as isize == -1 || a.is_null() {
        return None;
    }
    Some(Mapping { addr: a, len })
}

// ---- the handle ----------------------------------------------------------------------------------------------------------------------

pub struct GuiComp {
    comp: Compositor<Mapping>,
    /// Events taken from the compositor and encoded, waiting to be popped.
    events: VecDeque<(ClientId, Vec<u8>)>,
    disconnects: VecDeque<ClientId>,
    fds: VecDeque<i32>,
    gpu_ops: VecDeque<GpuOp>,
    /// The draw list built last, and the titles of its TITLE operations (so the C side can read them as strings).
    draw: Vec<DrawOp>,
    titles: Vec<Option<String>>,
}

impl GuiComp {
    /// Moves what the compositor queued into this handle's queues (events encoded here, once). Every `gui_pop_*` does it first, so the
    /// calls that change the compositor need not.
    fn collect(&mut self) {
        for (c, ev) in self.comp.take_events() {
            let mut e = Encoder::new();
            ev.encode(&mut e);
            let (bytes, _) = e.take();
            self.events.push_back((c, bytes));
        }
        self.disconnects.extend(self.comp.take_disconnects());
        self.fds.extend(self.comp.take_fds_to_close());
        self.gpu_ops.extend(self.comp.take_gpu_ops());
    }
}

#[repr(C)]
pub struct GuiGpuOp {
    kind: u32,
    _pad: u32,
    handle: u64,
    fd: i32,
    width: i32,
    height: i32,
    stride: u32,
    size: u64,
}

#[repr(C)]
#[derive(Default)]
pub struct GuiDrawOp {
    kind: u32,
    color: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    sx: i32,
    sy: i32,
    handle: u64,
    client: u32,
    surface: u32,
    version: u64,
    src_w: i32,
    src_h: i32,
    id: u32,
    focused: u32,
    clip_x: i32,
    clip_y: i32,
    clip_w: i32,
    clip_h: i32,
    title_fg: u32,
    title_shadow: u32,
    shape_radius: f32,
    shape_border: f32,
    shape_split: f32,
    shape_shadow_blur: f32,
    shape_c: [u32; 4],
    shape_border_color: u32,
    shape_shadow_color: u32,
    shape_shadow_dx: i32,
    shape_shadow_dy: i32,
    shape_horizontal: u32,
    premul: u32,
}

unsafe fn get<'a>(c: *mut GuiComp) -> &'a mut GuiComp {
    &mut *c
}

#[no_mangle]
pub extern "C" fn gui_new(width: i32, height: i32) -> *mut GuiComp {
    Box::into_raw(Box::new(GuiComp {
        comp: Compositor::new(width, height),
        events: VecDeque::new(),
        disconnects: VecDeque::new(),
        fds: VecDeque::new(),
        gpu_ops: VecDeque::new(),
        draw: Vec::new(),
        titles: Vec::new(),
    }))
}

#[no_mangle]
pub unsafe extern "C" fn gui_free(c: *mut GuiComp) {
    if !c.is_null() {
        drop(Box::from_raw(c));
    }
}

#[no_mangle]
pub unsafe extern "C" fn gui_enable_gpu_buffers(c: *mut GuiComp) {
    get(c).comp.enable_gpu_buffers();
}

#[no_mangle]
pub unsafe extern "C" fn gui_add_client(c: *mut GuiComp) -> u32 {
    get(c).comp.add_client()
}

#[no_mangle]
pub unsafe extern "C" fn gui_remove_client(c: *mut GuiComp, client: u32) {
    let g = get(c);
    g.comp.remove_client(client);
}

#[no_mangle]
pub unsafe extern "C" fn gui_client_data(c: *mut GuiComp, client: u32, bytes: *const u8, len: usize, fds: *const i32, nfds: usize) {
    let g = get(c);
    let b = if len == 0 { &[][..] } else { core::slice::from_raw_parts(bytes, len) };
    let f = if nfds == 0 { &[][..] } else { core::slice::from_raw_parts(fds, nfds) };
    g.comp.client_data(client, b, f, &mut map_pool);
}

#[no_mangle]
pub unsafe extern "C" fn gui_pop_event(c: *mut GuiComp, client: *mut u32, buf: *mut u8, cap: usize) -> usize {
    let g = get(c);
    g.collect();
    match g.events.front() {
        Some((_, bytes)) if bytes.len() <= cap => {
            let (cl, bytes) = g.events.pop_front().unwrap();
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, bytes.len());
            *client = cl;
            bytes.len()
        }
        _ => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn gui_pop_disconnect(c: *mut GuiComp, client: *mut u32) -> c_int {
    let g = get(c);
    g.collect();
    match g.disconnects.pop_front() {
        Some(cl) => {
            *client = cl;
            1
        }
        None => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn gui_pop_fd_to_close(c: *mut GuiComp, fd: *mut i32) -> c_int {
    let g = get(c);
    g.collect();
    match g.fds.pop_front() {
        Some(f) => {
            *fd = f;
            1
        }
        None => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn gui_pop_gpu_op(c: *mut GuiComp, op: *mut GuiGpuOp) -> c_int {
    let g = get(c);
    g.collect();
    match g.gpu_ops.pop_front() {
        Some(GpuOp::Import { handle, fd, size, width, height, stride }) => {
            *op = GuiGpuOp { kind: 1, _pad: 0, handle, fd, width, height, stride: stride as u32, size: size as u64 };
            1
        }
        Some(GpuOp::Drop { handle }) => {
            *op = GuiGpuOp { kind: 2, _pad: 0, handle, fd: -1, width: 0, height: 0, stride: 0, size: 0 };
            1
        }
        None => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn gui_set_time(c: *mut GuiComp, ms: u32) {
    get(c).comp.set_time(ms);
}

#[no_mangle]
pub unsafe extern "C" fn gui_pointer_motion(c: *mut GuiComp, dx: i32, dy: i32) {
    let g = get(c);
    g.comp.pointer_motion(dx, dy);
}

#[no_mangle]
pub unsafe extern "C" fn gui_pointer_button(c: *mut GuiComp, code: u32, pressed: c_int) {
    let g = get(c);
    g.comp.pointer_button(code, pressed != 0);
}

#[no_mangle]
pub unsafe extern "C" fn gui_key(c: *mut GuiComp, code: u32, pressed: c_int) {
    let g = get(c);
    g.comp.key(code, pressed != 0);
}

#[no_mangle]
pub unsafe extern "C" fn gui_quit_requested(c: *const GuiComp) -> c_int {
    (*c).comp.quit_requested() as c_int
}

#[no_mangle]
pub unsafe extern "C" fn gui_frame_done(c: *mut GuiComp, ms: u32) {
    let g = get(c);
    g.comp.frame_done(ms);
}

#[no_mangle]
pub unsafe extern "C" fn gui_has_frame_callbacks(c: *const GuiComp) -> c_int {
    (*c).comp.has_frame_callbacks() as c_int
}

#[no_mangle]
pub unsafe extern "C" fn gui_has_damage(c: *const GuiComp) -> c_int {
    (*c).comp.has_damage() as c_int
}

#[no_mangle]
pub unsafe extern "C" fn gui_set_theme(c: *mut GuiComp, name: *const c_char) -> c_int {
    let Ok(name) = core::ffi::CStr::from_ptr(name).to_str() else { return -1 };
    match gui::theme::by_name(name) {
        Some(t) => {
            get(c).comp.set_theme(t);
            0
        }
        None => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn gui_draw_list(c: *mut GuiComp) -> u64 {
    let g = get(c);
    let (epoch, ops) = g.comp.draw_list();
    g.titles = ops.iter().map(|o| if let DrawOp::Title { title, .. } = o { Some(alloc::format!("{}\0", title)) } else { None }).collect();
    g.draw = ops;
    epoch
}

#[no_mangle]
pub unsafe extern "C" fn gui_draw_count(c: *const GuiComp) -> usize {
    (*c).draw.len()
}

#[no_mangle]
pub unsafe extern "C" fn gui_draw_get(c: *const GuiComp, i: usize, out: *mut GuiDrawOp) -> c_int {
    let Some(op) = (&(*c).draw).get(i) else { return -1 };
    let mut o = GuiDrawOp::default();
    match op {
        DrawOp::Fill { rect, color } => {
            o.kind = 0;
            o.color = *color;
            (o.x, o.y, o.w, o.h) = (rect.x, rect.y, rect.w, rect.h);
        }
        DrawOp::Gpu { handle, dst, sx, sy } => {
            o.kind = 1;
            o.handle = *handle;
            (o.x, o.y, o.w, o.h) = (dst.x, dst.y, dst.w, dst.h);
            (o.sx, o.sy) = (*sx, *sy);
        }
        DrawOp::Cpu { client, surface, version, dst, sx, sy, w, h, premul } => {
            o.kind = 2;
            o.premul = *premul as u32;
            (o.client, o.surface, o.version) = (*client, *surface, *version);
            (o.x, o.y, o.w, o.h) = (dst.x, dst.y, dst.w, dst.h);
            (o.sx, o.sy, o.src_w, o.src_h) = (*sx, *sy, *w, *h);
        }
        DrawOp::Title { id, focused, fg, shadow, area, clip, .. } => {
            o.kind = 3;
            (o.title_fg, o.title_shadow) = (*fg, *shadow);
            o.id = *id;
            o.focused = *focused as u32;
            (o.x, o.y, o.w, o.h) = (area.x, area.y, area.w, area.h);
            (o.clip_x, o.clip_y, o.clip_w, o.clip_h) = (clip.x, clip.y, clip.w, clip.h);
        }
        DrawOp::Cursor { x, y } => {
            o.kind = 4;
            (o.x, o.y) = (*x, *y);
        }
        DrawOp::Shape { rect, shape } => {
            o.kind = 5;
            (o.x, o.y, o.w, o.h) = (rect.x, rect.y, rect.w, rect.h);
            (o.shape_radius, o.shape_border, o.shape_split, o.shape_shadow_blur) = (shape.radius, shape.border, shape.split, shape.shadow_blur);
            (o.shape_c, o.shape_border_color, o.shape_shadow_color) = (shape.c, shape.border_color, shape.shadow_color);
            (o.shape_shadow_dx, o.shape_shadow_dy, o.shape_horizontal) = (shape.shadow_dx, shape.shadow_dy, shape.horizontal as u32);
        }
    }
    *out = o;
    0
}

#[no_mangle]
pub unsafe extern "C" fn gui_title(c: *const GuiComp, i: usize) -> *const c_char {
    match (&(*c).titles).get(i) {
        Some(Some(s)) => s.as_ptr() as *const c_char,
        _ => core::ptr::null(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn gui_cpu_content(c: *const GuiComp, client: u32, surface: u32, len: *mut usize) -> *const u32 {
    match (*c).comp.cpu_content(client, surface) {
        Some(px) => {
            *len = px.len();
            px.as_ptr()
        }
        None => core::ptr::null(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn gui_gpu_frame_done(c: *mut GuiComp, epoch: u64) {
    let g = get(c);
    g.comp.gpu_frame_done(epoch);
}

#[no_mangle]
pub extern "C" fn gui_cursor_bitmap(row: usize) -> *const c_char {
    match CURSOR.get(row) {
        Some(r) => r.as_ptr() as *const c_char,
        None => core::ptr::null(),
    }
}
