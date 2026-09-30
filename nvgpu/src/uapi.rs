//! `nvgpu::uapi` — the Rust mirror of `nvgpu/uapi/nvgpu.h`, the interface of `/dev/nvgpu` (plan: `docs/gpu/g4-nvkmd-plan.md`).
//!
//! The header is the contract; every size, offset and ioctl number below is checked in the tests against the output of
//! `nvgpu/gen/uapi.c` compiled by clang:
//!
//! ```text
//! clang -I nvgpu/uapi -Wall -Wextra -o /tmp/uapi nvgpu/gen/uapi.c && /tmp/uapi
//! ```

use core::mem::size_of;

pub const ABI_VERSION: u32 = 1;
pub const INFO_SOFTWARE: u32 = 1 << 0;

pub const BO_SYSTEM: u32 = 0;
pub const BO_VRAM: u32 = 1;

pub const ENGINE_COPY: u32 = 1 << 0;
pub const ENGINE_2D: u32 = 1 << 1;
pub const ENGINE_3D: u32 = 1 << 2;
pub const ENGINE_M2MF: u32 = 1 << 3;
pub const ENGINE_COMPUTE: u32 = 1 << 4;
pub const ENGINE_ALL: u32 = ENGINE_COPY | ENGINE_2D | ENGINE_3D | ENGINE_M2MF | ENGINE_COMPUTE;

pub const PUSH_NO_PREFETCH: u32 = 1 << 0;
/// Largest push segment: a GPFIFO entry's length field is 21 bits of words.
pub const PUSH_MAX_BYTES: u32 = 0x7f_fffc;

pub const WAIT_ANY: u32 = 1 << 0;
pub const WAIT_PENDING: u32 = 1 << 1;

/// Linux's `_IOWR('N', nr, size)`.
pub const fn ioc(nr: u32, size: usize) -> u32 {
    0xC000_0000 | ((size as u32) << 16) | ((b'N' as u32) << 8) | nr
}

pub const IOC_INFO: u32 = ioc(1, size_of::<Info>());
pub const IOC_BO_CREATE: u32 = ioc(2, size_of::<BoCreate>());
pub const IOC_BO_FREE: u32 = ioc(3, size_of::<BoFree>());
pub const IOC_VA_ALLOC: u32 = ioc(4, size_of::<VaAlloc>());
pub const IOC_VA_FREE: u32 = ioc(5, size_of::<VaFree>());
pub const IOC_VA_BIND: u32 = ioc(6, size_of::<VaBind>());
pub const IOC_VA_UNBIND: u32 = ioc(7, size_of::<VaUnbind>());
pub const IOC_CTX_CREATE: u32 = ioc(8, size_of::<CtxCreate>());
pub const IOC_CTX_DESTROY: u32 = ioc(9, size_of::<CtxDestroy>());
pub const IOC_EXEC: u32 = ioc(10, size_of::<Exec>());
pub const IOC_SYNC_CREATE: u32 = ioc(11, size_of::<SyncCreate>());
pub const IOC_SYNC_DESTROY: u32 = ioc(12, size_of::<SyncDestroy>());
pub const IOC_SYNC_SIGNAL: u32 = ioc(13, size_of::<SyncSignal>());
pub const IOC_SYNC_WAIT: u32 = ioc(14, size_of::<SyncWait>());
pub const IOC_SYNC_QUERY: u32 = ioc(15, size_of::<SyncQuery>());
pub const IOC_TIMESTAMP: u32 = ioc(16, size_of::<Timestamp>());

/// `struct nvg_info`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Info {
    pub abi_version: u32,
    pub flags: u32,
    pub device_id: u16,
    pub chipset: u16,
    pub sm: u8,
    pub gpc_count: u8,
    pub tpc_count: u16,
    pub mp_per_tpc: u8,
    pub max_warps_per_mp: u8,
    pub max_blocks_per_mp: u8,
    pub _pad0: u8,
    pub cls_copy: u16,
    pub cls_eng2d: u16,
    pub cls_eng3d: u16,
    pub cls_m2mf: u16,
    pub cls_compute: u16,
    pub cls_gpfifo: u16,
    pub cls_vdec: u16,
    pub max_smem_per_wg_kb: u16,
    pub vram_size_b: u64,
    pub vram_used_b: u64,
    pub bar_size_b: u64,
    pub va_start: u64,
    pub va_end: u64,
    pub device_name: [u8; 64],
    pub chipset_name: [u8; 16],
}

/// `struct nvg_bo_create`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct BoCreate {
    pub size: u64,
    pub flags: u32,
    pub handle: u32,
    pub mmap_offset: u64,
    pub size_out: u64,
}

/// `struct nvg_bo_free`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct BoFree {
    pub handle: u32,
    pub _pad: u32,
}

/// `struct nvg_va_alloc`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VaAlloc {
    pub size: u64,
    pub align: u64,
    pub va: u64,
    pub flags: u64,
}

/// `struct nvg_va_free`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VaFree {
    pub va: u64,
    pub size: u64,
}

/// `struct nvg_va_bind`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VaBind {
    pub va: u64,
    pub size: u64,
    pub bo_offset: u64,
    pub handle: u32,
    pub pte_kind: u32,
}

/// `struct nvg_va_unbind`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VaUnbind {
    pub va: u64,
    pub size: u64,
}

/// `struct nvg_ctx_create`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CtxCreate {
    pub engines: u32,
    pub ctx: u32,
}

/// `struct nvg_ctx_destroy`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CtxDestroy {
    pub ctx: u32,
    pub _pad: u32,
}

/// `struct nvg_push`.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Push {
    pub va: u64,
    pub bytes: u32,
    pub flags: u32,
}

/// `struct nvg_sync_ref`.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct SyncRef {
    pub handle: u32,
    pub _pad: u32,
    pub value: u64,
}

/// `struct nvg_exec`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Exec {
    pub ctx: u32,
    pub push_count: u32,
    pub wait_count: u32,
    pub sig_count: u32,
    pub pushes: u64,
    pub waits: u64,
    pub signals: u64,
}

/// `struct nvg_sync_create`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SyncCreate {
    pub initial: u64,
    pub handle: u32,
    pub _pad: u32,
}

/// `struct nvg_sync_destroy`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SyncDestroy {
    pub handle: u32,
    pub _pad: u32,
}

/// `struct nvg_sync_signal`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SyncSignal {
    pub handle: u32,
    pub _pad: u32,
    pub value: u64,
}

/// `struct nvg_sync_wait`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SyncWait {
    pub refs: u64,
    pub count: u32,
    pub flags: u32,
    pub first_ready: u32,
    pub _pad: u32,
}

/// `struct nvg_sync_query`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SyncQuery {
    pub handle: u32,
    pub _pad: u32,
    pub value: u64,
    pub pending: u64,
}

/// `struct nvg_timestamp`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Timestamp {
    pub ns: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;

    #[test]
    fn layouts_match_the_c_header() {
        assert_eq!(size_of::<Info>(), 160, "nvg_info");
        assert_eq!(size_of::<BoCreate>(), 32, "nvg_bo_create");
        assert_eq!(size_of::<BoFree>(), 8, "nvg_bo_free");
        assert_eq!(size_of::<VaAlloc>(), 32, "nvg_va_alloc");
        assert_eq!(size_of::<VaFree>(), 16, "nvg_va_free");
        assert_eq!(size_of::<VaBind>(), 32, "nvg_va_bind");
        assert_eq!(size_of::<VaUnbind>(), 16, "nvg_va_unbind");
        assert_eq!(size_of::<CtxCreate>(), 8, "nvg_ctx_create");
        assert_eq!(size_of::<CtxDestroy>(), 8, "nvg_ctx_destroy");
        assert_eq!(size_of::<Push>(), 16, "nvg_push");
        assert_eq!(size_of::<SyncRef>(), 16, "nvg_sync_ref");
        assert_eq!(size_of::<Exec>(), 40, "nvg_exec");
        assert_eq!(size_of::<SyncCreate>(), 16, "nvg_sync_create");
        assert_eq!(size_of::<SyncDestroy>(), 8, "nvg_sync_destroy");
        assert_eq!(size_of::<SyncSignal>(), 16, "nvg_sync_signal");
        assert_eq!(size_of::<SyncWait>(), 24, "nvg_sync_wait");
        assert_eq!(size_of::<SyncQuery>(), 24, "nvg_sync_query");
        assert_eq!(size_of::<Timestamp>(), 8, "nvg_timestamp");
        assert_eq!(offset_of!(Info, abi_version), 0, "nvg_info.abi_version");
        assert_eq!(offset_of!(Info, flags), 4, "nvg_info.flags");
        assert_eq!(offset_of!(Info, device_id), 8, "nvg_info.device_id");
        assert_eq!(offset_of!(Info, sm), 12, "nvg_info.sm");
        assert_eq!(offset_of!(Info, gpc_count), 13, "nvg_info.gpc_count");
        assert_eq!(offset_of!(Info, tpc_count), 14, "nvg_info.tpc_count");
        assert_eq!(offset_of!(Info, mp_per_tpc), 16, "nvg_info.mp_per_tpc");
        assert_eq!(offset_of!(Info, cls_copy), 20, "nvg_info.cls_copy");
        assert_eq!(offset_of!(Info, cls_compute), 28, "nvg_info.cls_compute");
        assert_eq!(offset_of!(Info, cls_vdec), 32, "nvg_info.cls_vdec");
        assert_eq!(offset_of!(Info, max_smem_per_wg_kb), 34, "nvg_info.max_smem_per_wg_kB");
        assert_eq!(offset_of!(Info, vram_size_b), 40, "nvg_info.vram_size_B");
        assert_eq!(offset_of!(Info, vram_used_b), 48, "nvg_info.vram_used_B");
        assert_eq!(offset_of!(Info, bar_size_b), 56, "nvg_info.bar_size_B");
        assert_eq!(offset_of!(Info, va_start), 64, "nvg_info.va_start");
        assert_eq!(offset_of!(Info, va_end), 72, "nvg_info.va_end");
        assert_eq!(offset_of!(Info, device_name), 80, "nvg_info.device_name");
        assert_eq!(offset_of!(Info, chipset_name), 144, "nvg_info.chipset_name");
        assert_eq!(offset_of!(BoCreate, flags), 8, "nvg_bo_create.flags");
        assert_eq!(offset_of!(BoCreate, handle), 12, "nvg_bo_create.handle");
        assert_eq!(offset_of!(BoCreate, mmap_offset), 16, "nvg_bo_create.mmap_offset");
        assert_eq!(offset_of!(BoCreate, size_out), 24, "nvg_bo_create.size_out");
        assert_eq!(offset_of!(VaAlloc, align), 8, "nvg_va_alloc.align");
        assert_eq!(offset_of!(VaAlloc, va), 16, "nvg_va_alloc.va");
        assert_eq!(offset_of!(VaAlloc, flags), 24, "nvg_va_alloc.flags");
        assert_eq!(offset_of!(VaBind, size), 8, "nvg_va_bind.size");
        assert_eq!(offset_of!(VaBind, bo_offset), 16, "nvg_va_bind.bo_offset");
        assert_eq!(offset_of!(VaBind, handle), 24, "nvg_va_bind.handle");
        assert_eq!(offset_of!(VaBind, pte_kind), 28, "nvg_va_bind.pte_kind");
        assert_eq!(offset_of!(Push, bytes), 8, "nvg_push.bytes");
        assert_eq!(offset_of!(Push, flags), 12, "nvg_push.flags");
        assert_eq!(offset_of!(SyncRef, value), 8, "nvg_sync_ref.value");
        assert_eq!(offset_of!(Exec, push_count), 4, "nvg_exec.push_count");
        assert_eq!(offset_of!(Exec, wait_count), 8, "nvg_exec.wait_count");
        assert_eq!(offset_of!(Exec, sig_count), 12, "nvg_exec.sig_count");
        assert_eq!(offset_of!(Exec, pushes), 16, "nvg_exec.pushes");
        assert_eq!(offset_of!(Exec, waits), 24, "nvg_exec.waits");
        assert_eq!(offset_of!(Exec, signals), 32, "nvg_exec.signals");
        assert_eq!(offset_of!(SyncSignal, value), 8, "nvg_sync_signal.value");
        assert_eq!(offset_of!(SyncWait, count), 8, "nvg_sync_wait.count");
        assert_eq!(offset_of!(SyncWait, flags), 12, "nvg_sync_wait.flags");
        assert_eq!(offset_of!(SyncWait, first_ready), 16, "nvg_sync_wait.first_ready");
        assert_eq!(offset_of!(SyncQuery, value), 8, "nvg_sync_query.value");
        assert_eq!(offset_of!(SyncQuery, pending), 16, "nvg_sync_query.pending");
    }

    #[test]
    fn ioctl_numbers_match_the_c_header() {
        assert_eq!(IOC_INFO, 0xc0a04e01, "NVG_IOC_INFO");
        assert_eq!(IOC_BO_CREATE, 0xc0204e02, "NVG_IOC_BO_CREATE");
        assert_eq!(IOC_BO_FREE, 0xc0084e03, "NVG_IOC_BO_FREE");
        assert_eq!(IOC_VA_ALLOC, 0xc0204e04, "NVG_IOC_VA_ALLOC");
        assert_eq!(IOC_VA_FREE, 0xc0104e05, "NVG_IOC_VA_FREE");
        assert_eq!(IOC_VA_BIND, 0xc0204e06, "NVG_IOC_VA_BIND");
        assert_eq!(IOC_VA_UNBIND, 0xc0104e07, "NVG_IOC_VA_UNBIND");
        assert_eq!(IOC_CTX_CREATE, 0xc0084e08, "NVG_IOC_CTX_CREATE");
        assert_eq!(IOC_CTX_DESTROY, 0xc0084e09, "NVG_IOC_CTX_DESTROY");
        assert_eq!(IOC_EXEC, 0xc0284e0a, "NVG_IOC_EXEC");
        assert_eq!(IOC_SYNC_CREATE, 0xc0104e0b, "NVG_IOC_SYNC_CREATE");
        assert_eq!(IOC_SYNC_DESTROY, 0xc0084e0c, "NVG_IOC_SYNC_DESTROY");
        assert_eq!(IOC_SYNC_SIGNAL, 0xc0104e0d, "NVG_IOC_SYNC_SIGNAL");
        assert_eq!(IOC_SYNC_WAIT, 0xc0184e0e, "NVG_IOC_SYNC_WAIT");
        assert_eq!(IOC_SYNC_QUERY, 0xc0184e0f, "NVG_IOC_SYNC_QUERY");
        assert_eq!(IOC_TIMESTAMP, 0xc0084e10, "NVG_IOC_TIMESTAMP");
    }

    #[test]
    fn every_struct_is_a_multiple_of_eight_bytes() {
        for s in [
            size_of::<Info>(), size_of::<BoCreate>(), size_of::<BoFree>(), size_of::<VaAlloc>(), size_of::<VaFree>(),
            size_of::<VaBind>(), size_of::<VaUnbind>(), size_of::<CtxCreate>(), size_of::<CtxDestroy>(), size_of::<Push>(),
            size_of::<SyncRef>(), size_of::<Exec>(), size_of::<SyncCreate>(), size_of::<SyncDestroy>(), size_of::<SyncSignal>(),
            size_of::<SyncWait>(), size_of::<SyncQuery>(), size_of::<Timestamp>(),
        ] {
            assert_eq!(s % 8, 0);
        }
    }

    #[test]
    fn ioctl_numbers_are_distinct() {
        let all = [
            IOC_INFO, IOC_BO_CREATE, IOC_BO_FREE, IOC_VA_ALLOC, IOC_VA_FREE, IOC_VA_BIND, IOC_VA_UNBIND, IOC_CTX_CREATE,
            IOC_CTX_DESTROY, IOC_EXEC, IOC_SYNC_CREATE, IOC_SYNC_DESTROY, IOC_SYNC_SIGNAL, IOC_SYNC_WAIT, IOC_SYNC_QUERY,
            IOC_TIMESTAMP,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
