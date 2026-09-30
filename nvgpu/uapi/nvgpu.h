/* SPDX-License-Identifier: MIT */
/*
 * /dev/nvgpu: constanos's interface to the NVIDIA GA106 for user space (Mesa's NVK, through the `nvkmd_constanos` backend).
 *
 * The plan and the reasoning: docs/gpu/g4-nvkmd-plan.md. The Rust mirror is `nvgpu::uapi` (nvgpu/src/uapi.rs); its tests check every
 * size and offset below against this file compiled by clang (nvgpu/gen/uapi.c).
 *
 * Rules: every struct is a multiple of 8 bytes with 8-byte members on 8-byte offsets (no implicit padding), all fields are
 * little-endian, an ioctl returns 0 or a negative errno (-EINVAL, -ENOENT, -ENOMEM, -ENOSPC, -EBUSY, -EEXIST, -EAGAIN, -EFAULT), and one process
 * at a time may hold the device open (a second open() fails with -EBUSY).
 */
#ifndef NVGPU_UAPI_H
#define NVGPU_UAPI_H

#include <stdint.h>

#define NVG_ABI_VERSION 1u

/* ioctl numbers: Linux's _IOWR('N', nr, struct), computed by hand so this header needs no <linux/ioctl.h>. */
#define NVG_IOC(nr, size) (0xC0000000u | ((uint32_t)(size) << 16) | ((uint32_t)'N' << 8) | (uint32_t)(nr))

/* ---- device information --------------------------------------------------------------------------------------------------- */

#define NVG_INFO_SOFTWARE (1u << 0) /* no GPU behind the device: memory and bookkeeping are real, execution is not */

struct nvg_info {
   uint32_t abi_version;
   uint32_t flags;        /* NVG_INFO_* */
   uint16_t device_id;
   uint16_t chipset;
   uint8_t sm;            /* shader model (86 for the GA106) */
   uint8_t gpc_count;
   uint16_t tpc_count;
   uint8_t mp_per_tpc;
   uint8_t max_warps_per_mp;
   uint8_t max_blocks_per_mp;
   uint8_t _pad0;
   uint16_t cls_copy;
   uint16_t cls_eng2d;
   uint16_t cls_eng3d;
   uint16_t cls_m2mf;
   uint16_t cls_compute;
   uint16_t cls_gpfifo;
   uint16_t cls_vdec;
   uint16_t max_smem_per_wg_kB;
   uint64_t vram_size_B;
   uint64_t vram_used_B;
   uint64_t bar_size_B;   /* CPU-visible VRAM: 0 (the CPU cannot read VRAM through BAR1 after GSP-RM boots) */
   uint64_t va_start;     /* first and one-past-last GPU virtual address user space may allocate */
   uint64_t va_end;
   char device_name[64];
   char chipset_name[16];
};
#define NVG_IOC_INFO NVG_IOC(1, sizeof(struct nvg_info))

/* ---- buffer objects --------------------------------------------------------------------------------------------------------- */

#define NVG_BO_SYSTEM 0u /* pages of system RAM: mappable by the CPU */
#define NVG_BO_VRAM 1u   /* device-local memory: never mappable by the CPU */

struct nvg_bo_create {
   uint64_t size;             /* in: bytes, rounded up to 4096 */
   uint32_t flags;            /* in: NVG_BO_SYSTEM or NVG_BO_VRAM */
   uint32_t handle;           /* out */
   uint64_t mmap_offset;      /* out: pass to mmap(2) on the device fd (system BOs only; ~0 for VRAM) */
   uint64_t size_out;         /* out: the size actually reserved */
};
#define NVG_IOC_BO_CREATE NVG_IOC(2, sizeof(struct nvg_bo_create))

struct nvg_bo_free {
   uint32_t handle;
   uint32_t _pad;
};
#define NVG_IOC_BO_FREE NVG_IOC(3, sizeof(struct nvg_bo_free))

/* ---- GPU virtual address space (one, shared by every context) --------------------------------------------------------------- */

struct nvg_va_alloc {
   uint64_t size;             /* in: bytes, rounded up to 4096 */
   uint64_t align;            /* in: power of two, at least 4096 */
   uint64_t va;               /* out */
   uint64_t flags;            /* in: 0 */
};
#define NVG_IOC_VA_ALLOC NVG_IOC(4, sizeof(struct nvg_va_alloc))

struct nvg_va_free {
   uint64_t va;
   uint64_t size;
};
#define NVG_IOC_VA_FREE NVG_IOC(5, sizeof(struct nvg_va_free))

/* Map [bo_offset, bo_offset + size) of a BO at [va, va + size), which must lie inside one allocated VA range and not overlap a
 * bound range. */
struct nvg_va_bind {
   uint64_t va;
   uint64_t size;
   uint64_t bo_offset;
   uint32_t handle;
   uint32_t pte_kind;         /* 0 = pitch/generic; other values pass through to the PTE */
};
#define NVG_IOC_VA_BIND NVG_IOC(6, sizeof(struct nvg_va_bind))

/* Unmap [va, va + size): any bound range, whole or in part (partial overlaps are split). Unbound gaps are fine. */
struct nvg_va_unbind {
   uint64_t va;
   uint64_t size;
};
#define NVG_IOC_VA_UNBIND NVG_IOC(7, sizeof(struct nvg_va_unbind))

/* ---- contexts and execution ------------------------------------------------------------------------------------------------- */

#define NVG_ENGINE_COPY (1u << 0)
#define NVG_ENGINE_2D (1u << 1)
#define NVG_ENGINE_3D (1u << 2)
#define NVG_ENGINE_M2MF (1u << 3)
#define NVG_ENGINE_COMPUTE (1u << 4)

struct nvg_ctx_create {
   uint32_t engines;          /* in: NVG_ENGINE_* the context will use */
   uint32_t ctx;              /* out */
};
#define NVG_IOC_CTX_CREATE NVG_IOC(8, sizeof(struct nvg_ctx_create))

struct nvg_ctx_destroy {
   uint32_t ctx;
   uint32_t _pad;
};
#define NVG_IOC_CTX_DESTROY NVG_IOC(9, sizeof(struct nvg_ctx_destroy))

/* One command segment: `bytes` of GPU push data at GPU virtual address `va`, inside bound memory. */
#define NVG_PUSH_NO_PREFETCH (1u << 0)
struct nvg_push {
   uint64_t va;
   uint32_t bytes;            /* a multiple of 4, at most 0x7ffffc */
   uint32_t flags;            /* NVG_PUSH_* */
};

struct nvg_sync_ref {
   uint32_t handle;
   uint32_t _pad;
   uint64_t value;            /* timeline value */
};

/* If every `waits` timeline has reached its value, queue `pushes` in order and set each `signals` timeline to its value when they
 * have completed; if not, do nothing and return -EAGAIN (the caller waits with NVG_IOC_SYNC_WAIT and tries again). The kernel never
 * blocks in an ioctl. */
struct nvg_exec {
   uint32_t ctx;
   uint32_t push_count;
   uint32_t wait_count;
   uint32_t sig_count;
   uint64_t pushes;           /* user pointer to push_count x struct nvg_push */
   uint64_t waits;            /* user pointer to wait_count x struct nvg_sync_ref */
   uint64_t signals;          /* user pointer to sig_count x struct nvg_sync_ref */
};
#define NVG_IOC_EXEC NVG_IOC(10, sizeof(struct nvg_exec))

/* ---- timeline synchronisation objects --------------------------------------------------------------------------------------- */

struct nvg_sync_create {
   uint64_t initial;          /* in: starting value */
   uint32_t handle;           /* out */
   uint32_t _pad;
};
#define NVG_IOC_SYNC_CREATE NVG_IOC(11, sizeof(struct nvg_sync_create))

struct nvg_sync_destroy {
   uint32_t handle;
   uint32_t _pad;
};
#define NVG_IOC_SYNC_DESTROY NVG_IOC(12, sizeof(struct nvg_sync_destroy))

struct nvg_sync_signal {   /* signal from the CPU; the value must not go backwards */
   uint32_t handle;
   uint32_t _pad;
   uint64_t value;
};
#define NVG_IOC_SYNC_SIGNAL NVG_IOC(13, sizeof(struct nvg_sync_signal))

#define NVG_WAIT_ANY (1u << 0)     /* succeed when any reference reached its value (default: all of them) */
#define NVG_WAIT_PENDING (1u << 1) /* compare with the timeline's pending value (see NVG_IOC_SYNC_QUERY) instead of its completed one */
/* Never blocks: 0 with `first_ready` set if the condition holds, -EAGAIN if not yet (the caller sleeps and asks again). */
struct nvg_sync_wait {
   uint64_t refs;             /* user pointer to count x struct nvg_sync_ref */
   uint32_t count;
   uint32_t flags;            /* NVG_WAIT_* */
   uint32_t first_ready;      /* out: index of a ready reference */
   uint32_t _pad;
};
#define NVG_IOC_SYNC_WAIT NVG_IOC(14, sizeof(struct nvg_sync_wait))

/* Every timeline has two values. `value` is what has completed (CPU signals and finished work). `pending` is the highest value any
 * EXEC already queued will signal, so pending >= value; a CPU signal raises both. Vulkan's threaded submit orders submissions with
 * it: it waits for `pending` before queuing work that waits for a value, so the wait can be satisfied without the CPU thread
 * blocking in the kernel. */
struct nvg_sync_query {
   uint32_t handle;
   uint32_t _pad;
   uint64_t value;            /* out: the timeline's completed value */
   uint64_t pending;          /* out: the timeline's pending value */
};
#define NVG_IOC_SYNC_QUERY NVG_IOC(15, sizeof(struct nvg_sync_query))

/* ---- clock ---------------------------------------------------------------------------------------------------------------- */

struct nvg_timestamp {
   uint64_t ns;               /* out: the GPU's global timer, in nanoseconds */
};
#define NVG_IOC_TIMESTAMP NVG_IOC(16, sizeof(struct nvg_timestamp))

#endif /* NVGPU_UAPI_H */
