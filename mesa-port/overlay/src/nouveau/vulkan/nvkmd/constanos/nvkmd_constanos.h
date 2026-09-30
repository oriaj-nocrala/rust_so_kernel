/*
 * nvkmd backend for constanos: NVK talking to /dev/nvgpu (nvgpu/uapi/nvgpu.h in the constanos tree; docs/gpu/g4-nvkmd-plan.md).
 * SPDX-License-Identifier: MIT
 */
#ifndef NVKMD_CONSTANOS_H
#define NVKMD_CONSTANOS_H 1

#include "nvkmd/nvkmd.h"
#include "vk_device.h"
#include "vk_sync.h"
#include "util/u_dynarray.h"
#include "util/vma.h"

#include "nvgpu.h"

#define NVKMD_CONSTANOS_DEVICE_PATH "/dev/nvgpu"

struct nvkmd_constanos_pdev {
   struct nvkmd_pdev base;

   struct vk_sync_type sync_type;
   const struct vk_sync_type *sync_types[2];
};

NVKMD_DECL_SUBCLASS(pdev, constanos);

/* Opens the device just long enough to read its description (it is exclusive: one holder at a time). */
VkResult nvkmd_constanos_try_create_pdev(struct vk_object_base *log_obj,
                                         enum nvk_debug debug_flags,
                                         struct nvkmd_pdev **pdev_out);

struct nvkmd_constanos_dev {
   struct nvkmd_dev base;

   /* The session: closing it releases everything the process holds on the GPU. */
   int fd;

   simple_mtx_t heap_mutex;
   struct util_vma_heap heap;
   struct util_vma_heap replay_heap;
};

NVKMD_DECL_SUBCLASS(dev, constanos);

VkResult nvkmd_constanos_create_dev(struct nvkmd_pdev *pdev,
                                    struct vk_object_base *log_obj,
                                    struct nvkmd_dev **dev_out);

struct nvkmd_constanos_mem {
   struct nvkmd_mem base;

   uint32_t handle;
   uint64_t mmap_offset;   /* ~0 for VRAM: never mappable by the CPU */
};

NVKMD_DECL_SUBCLASS(mem, constanos);

struct nvkmd_constanos_va {
   struct nvkmd_va base;
};

NVKMD_DECL_SUBCLASS(va, constanos);

#define NVKMD_CONSTANOS_MAX_PUSHES 256
#define NVKMD_CONSTANOS_MAX_SYNCS 64

struct nvkmd_constanos_exec_ctx {
   struct nvkmd_ctx base;

   int fd;
   uint32_t ctx;

   /* A private timeline: sync() signals the next value and waits for it. */
   uint32_t sync;
   uint64_t sync_value;

   uint32_t push_count, wait_count, sig_count;
   struct nvg_push pushes[NVKMD_CONSTANOS_MAX_PUSHES];
   struct nvg_sync_ref waits[NVKMD_CONSTANOS_MAX_SYNCS];
   struct nvg_sync_ref sigs[NVKMD_CONSTANOS_MAX_SYNCS];
};

NVKMD_DECL_SUBCLASS(ctx, constanos_exec);

/* Binds are synchronous ioctls, so a bind context has no queue: it waits for its waits, binds, then signals. */
struct nvkmd_constanos_bind_ctx {
   struct nvkmd_ctx base;

   int fd;

   uint32_t wait_count;
   struct nvg_sync_ref waits[NVKMD_CONSTANOS_MAX_SYNCS];
};

NVKMD_DECL_SUBCLASS(ctx, constanos_bind);

VkResult nvkmd_constanos_create_ctx(struct nvkmd_dev *dev,
                                    struct vk_object_base *log_obj,
                                    enum nvkmd_engines engines,
                                    struct nvkmd_ctx **ctx_out);

VkResult nvkmd_constanos_alloc_va(struct nvkmd_dev *dev,
                                  struct vk_object_base *log_obj,
                                  enum nvkmd_va_flags flags, uint8_t pte_kind,
                                  uint64_t size_B, uint64_t align_B,
                                  uint64_t fixed_addr, struct nvkmd_va **va_out);

/* vk_device::copy_sync_payloads for timelines that are kernel objects. */
VkResult nvkmd_constanos_copy_sync_payloads(struct vk_device *device,
                                            uint32_t wait_count,
                                            const struct vk_sync_wait *waits,
                                            uint32_t signal_count,
                                            const struct vk_sync_signal *signals);

/* constanos extension for programs that link NVK statically (not Vulkan): the screen. See nvkmd_constanos.c. */
int nvk_constanos_scanout_info(VkDevice device, struct nvg_scanout_info *out);
int nvk_constanos_present(VkDevice device, VkDeviceMemory memory, uint64_t offset);
int nvk_constanos_flip_pending(VkDevice device);

#endif /* NVKMD_CONSTANOS_H */
