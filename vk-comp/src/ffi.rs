//! The renderer (`probes/nvk/comp_vk.c`, `comp_api.h`): C on NVK. Plain data across the line.

use std::ffi::c_int;

pub const CR_FILL: u32 = 0;
pub const CR_GPU: u32 = 1;
pub const CR_CPU: u32 = 2;
#[allow(dead_code)] // the window manager emits shapes from step 2 of docs/gui/compositor-visual-plan.md
pub const CR_SHAPE: u32 = 3;

pub const CR_OPAQUE: u32 = 0;
pub const CR_KEYED: u32 = 1;
#[allow(dead_code)] // icons, step 4
pub const CR_PREMUL: u32 = 2;

/// `struct cr_shape`: colours `0xAARRGGBB`, straight alpha.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CrShape {
    pub radius: f32,
    pub border: f32,
    pub split: f32,
    pub shadow_blur: f32,
    pub c: [u32; 4],
    pub border_color: u32,
    pub shadow_color: u32,
    pub shadow_dx: i32,
    pub shadow_dy: i32,
    pub horizontal: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CrOp {
    pub kind: u32,
    pub color: u32,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub sx: i32,
    pub sy: i32,
    pub key: u64,
    pub version: u64,
    pub px: *const u32,
    pub npx: u64,
    pub src_w: i32,
    pub alpha: u32,
    pub shape: CrShape,
}

impl CrOp {
    pub const fn new(kind: u32) -> CrOp {
        CrOp { kind, color: 0, x: 0, y: 0, w: 0, h: 0, sx: 0, sy: 0, key: 0, version: 0, px: std::ptr::null(), npx: 0, src_w: 0, alpha: CR_OPAQUE, shape: CrShape {
            radius: 0.0, border: 0.0, split: 1.0, shadow_blur: 0.0, c: [0; 4], border_color: 0, shadow_color: 0, shadow_dx: 0, shadow_dy: 0, horizontal: 0,
        } }
    }
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct CrStats {
    pub frames: u32,
    pub draws: u32,
    pub draws_max: u32,
    pub imports: u32,
    pub drops: u32,
    pub uploads: u32,
    pub acquire_us: u32,
    pub render_us: u32,
    pub present_us: u32,
    pub upload_kb: u32,
}

extern "C" {
    pub fn cr_init(headless: c_int, width: *mut u32, height: *mut u32) -> c_int;
    pub fn cr_import(handle: u64, fd: c_int, size: u64, stride_bytes: u32) -> c_int;
    pub fn cr_drop(handle: u64);
    pub fn cr_wait() -> u64;
    pub fn cr_frame(ops: *const CrOp, n: usize, epoch: u64) -> c_int;
    pub fn cr_wait_flip() -> c_int;
    pub fn cr_get_stats(out: *mut CrStats);
    pub fn cr_shutdown();
}
