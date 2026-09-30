/*
 * nvkmd backend for constanos.
 *
 * The kernel side is /dev/nvgpu (nvgpu/uapi/nvgpu.h, model in nvgpu/src/devmodel.rs); the reasoning is in docs/gpu/g4-nvkmd-plan.md.
 * What sets it apart from the nouveau backend:
 *
 *  - The kernel never blocks in an ioctl. A wait that is not satisfied comes back as EAGAIN and this file sleeps and asks again
 *    (wait_refs); an EXEC whose waits are not ready does nothing and is retried the same way.
 *  - The VA heap is ours (util_vma_heap over the whole user range, which one VA_ALLOC reserves at device creation); the kernel
 *    only checks that binds lie inside it and do not overlap.
 *  - The CPU cannot read VRAM (BAR1 is write-only after GSP-RM boots), so anything that may be mapped (NVKMD_MEM_CAN_MAP) is put
 *    in system memory, which the GPU reaches over PCIe; the rest of "local" memory is VRAM.
 *  - Synchronisation objects are kernel timelines with a `pending` value, which is what Vulkan's WAIT_PENDING needs.
 *
 * SPDX-License-Identifier: MIT
 */
#include "nvkmd_constanos.h"

#include "nvk_device.h"

#include "vk_alloc.h"
#include "vk_log.h"
#include "vk_sync.h"
#include "vk_util.h"

#include "util/os_time.h"
#include "util/u_math.h"
#include "util/u_memory.h"

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

/* Where user space may allocate GPU virtual addresses is decided by the kernel (nvg_info.va_start/va_end); this splits it. */
#define REPLAY_HEAP_FRACTION 8 /* the top eighth is the capture/replay heap */

static inline int
constanos_ioctl(int fd, unsigned long request, void *arg)
{
   int r;
   do {
      r = ioctl(fd, request, arg);
   } while (r < 0 && errno == EINTR);
   return r < 0 ? -errno : 0;
}

/* ---- waiting -------------------------------------------------------------------------------------------------------------- */

/* Ask until `refs` satisfy `flags` (NVG_WAIT_*) or `abs_timeout_ns` (os_time_get_nano() clock; UINT64_MAX = no limit) passes.
 * Returns 0, -ETIMEDOUT or -errno. The kernel takes at most NVKMD_CONSTANOS_MAX_SYNCS references per call, so longer lists are
 * asked in slices. */
static int
wait_refs(int fd, const struct nvg_sync_ref *refs, uint32_t count, uint32_t flags, uint64_t abs_timeout_ns)
{
   const bool any = flags & NVG_WAIT_ANY;
   uint32_t sleep_us = 20;

   for (;;) {
      bool all_ready = true;
      for (uint32_t i = 0; i < count; i += NVKMD_CONSTANOS_MAX_SYNCS) {
         const uint32_t n = MIN2(NVKMD_CONSTANOS_MAX_SYNCS, count - i);
         struct nvg_sync_wait w = {
            .refs = (uintptr_t)(refs + i),
            .count = n,
            .flags = flags,
         };
         const int r = constanos_ioctl(fd, NVG_IOC_SYNC_WAIT, &w);
         if (r == 0) {
            if (any)
               return 0;
         } else if (r == -EAGAIN) {
            all_ready = false;
         } else {
            return r;
         }
      }
      if (all_ready && !any)
         return 0;

      if (abs_timeout_ns != UINT64_MAX && os_time_get_nano() >= abs_timeout_ns)
         return -ETIMEDOUT;

      os_time_sleep(sleep_us);
      sleep_us = MIN2(sleep_us * 2, 500);
   }
}

/* ---- timelines as vk_sync ------------------------------------------------------------------------------------------------- */

struct nvkmd_constanos_sync {
   struct vk_sync base;
   uint32_t handle;
};

static inline struct nvkmd_constanos_sync *
to_sync(struct vk_sync *sync)
{
   return container_of(sync, struct nvkmd_constanos_sync, base);
}

/* The vk_sync functions only get the vk_device; the session is the nvkmd device's. */
static int
device_fd(struct vk_device *vk)
{
   struct nvk_device *dev = container_of(vk, struct nvk_device, vk);
   return nvkmd_constanos_dev(dev->nvkmd)->fd;
}

/* Binary syncs are timelines that only ever reach 1. */
static inline uint64_t
sync_value(const struct vk_sync *sync, uint64_t value)
{
   return (sync->flags & VK_SYNC_IS_TIMELINE) ? value : 1;
}

static VkResult
sync_create_handle(struct vk_device *device, uint64_t initial, uint32_t *handle_out)
{
   struct nvg_sync_create c = { .initial = initial };
   if (constanos_ioctl(device_fd(device), NVG_IOC_SYNC_CREATE, &c) != 0)
      return vk_error(device, VK_ERROR_OUT_OF_HOST_MEMORY);
   *handle_out = c.handle;
   return VK_SUCCESS;
}

static void
sync_destroy_handle(struct vk_device *device, uint32_t handle)
{
   struct nvg_sync_destroy d = { .handle = handle };
   ASSERTED int r = constanos_ioctl(device_fd(device), NVG_IOC_SYNC_DESTROY, &d);
   assert(r == 0);
}

static VkResult
constanos_sync_init(struct vk_device *device, struct vk_sync *sync, uint64_t initial_value)
{
   struct nvkmd_constanos_sync *s = to_sync(sync);
   const uint64_t initial = (sync->flags & VK_SYNC_IS_TIMELINE) ? initial_value : (initial_value != 0);
   return sync_create_handle(device, initial, &s->handle);
}

static void
constanos_sync_finish(struct vk_device *device, struct vk_sync *sync)
{
   sync_destroy_handle(device, to_sync(sync)->handle);
}

static VkResult
constanos_sync_signal(struct vk_device *device, struct vk_sync *sync, uint64_t value)
{
   struct nvg_sync_signal s = { .handle = to_sync(sync)->handle, .value = sync_value(sync, value) };
   if (constanos_ioctl(device_fd(device), NVG_IOC_SYNC_SIGNAL, &s) != 0)
      return vk_error(device, VK_ERROR_UNKNOWN);
   return VK_SUCCESS;
}

static VkResult
constanos_sync_get_value(struct vk_device *device, struct vk_sync *sync, uint64_t *value)
{
   struct nvg_sync_query q = { .handle = to_sync(sync)->handle };
   if (constanos_ioctl(device_fd(device), NVG_IOC_SYNC_QUERY, &q) != 0)
      return vk_error(device, VK_ERROR_UNKNOWN);
   *value = q.value;
   return VK_SUCCESS;
}

/* A timeline cannot go backwards, so resetting a binary sync gives it a fresh one. */
static VkResult
constanos_sync_reset(struct vk_device *device, struct vk_sync *sync)
{
   struct nvkmd_constanos_sync *s = to_sync(sync);
   uint32_t fresh;
   VkResult result = sync_create_handle(device, 0, &fresh);
   if (result != VK_SUCCESS)
      return result;
   sync_destroy_handle(device, s->handle);
   s->handle = fresh;
   return VK_SUCCESS;
}

static VkResult
constanos_sync_move(struct vk_device *device, struct vk_sync *dst, struct vk_sync *src)
{
   struct nvkmd_constanos_sync *d = to_sync(dst);
   struct nvkmd_constanos_sync *s = to_sync(src);
   uint32_t fresh;
   VkResult result = sync_create_handle(device, 0, &fresh);
   if (result != VK_SUCCESS)
      return result;
   sync_destroy_handle(device, d->handle);
   d->handle = s->handle;
   s->handle = fresh;
   return VK_SUCCESS;
}

static VkResult
constanos_sync_wait_many(struct vk_device *device, uint32_t wait_count, const struct vk_sync_wait *waits,
                         enum vk_sync_wait_flags wait_flags, uint64_t abs_timeout_ns)
{
   STACK_ARRAY(struct nvg_sync_ref, refs, wait_count);
   uint32_t n = 0;
   for (uint32_t i = 0; i < wait_count; i++) {
      /* A wait for 0 on a timeline is a no-op (with ANY it is ready at once). */
      const uint64_t v = sync_value(waits[i].sync, waits[i].wait_value);
      if (v == 0) {
         if (wait_flags & VK_SYNC_WAIT_ANY) {
            STACK_ARRAY_FINISH(refs);
            return VK_SUCCESS;
         }
         continue;
      }
      refs[n++] = (struct nvg_sync_ref){ .handle = to_sync(waits[i].sync)->handle, .value = v };
   }

   int r = 0;
   if (n > 0) {
      uint32_t flags = 0;
      if (wait_flags & VK_SYNC_WAIT_ANY)
         flags |= NVG_WAIT_ANY;
      if (wait_flags & VK_SYNC_WAIT_PENDING)
         flags |= NVG_WAIT_PENDING;
      r = wait_refs(device_fd(device), refs, n, flags, abs_timeout_ns);
   }
   STACK_ARRAY_FINISH(refs);

   if (r == -ETIMEDOUT)
      return VK_TIMEOUT;
   if (r != 0)
      return vk_errorf(device, VK_ERROR_UNKNOWN, "sync wait failed: %s", strerror(-r));
   return VK_SUCCESS;
}

static VkResult
constanos_copy_payloads(struct vk_device *device, uint32_t wait_count, const struct vk_sync_wait *waits,
                        uint32_t signal_count, const struct vk_sync_signal *signals)
{
   VkResult result = constanos_sync_wait_many(device, wait_count, waits, 0, UINT64_MAX);
   if (result != VK_SUCCESS)
      return result;
   for (uint32_t i = 0; i < signal_count; i++) {
      result = constanos_sync_signal(device, signals[i].sync, signals[i].signal_value);
      if (result != VK_SUCCESS)
         return result;
   }
   return VK_SUCCESS;
}

VkResult
nvkmd_constanos_copy_sync_payloads(struct vk_device *device, uint32_t wait_count, const struct vk_sync_wait *waits,
                                   uint32_t signal_count, const struct vk_sync_signal *signals)
{
   return constanos_copy_payloads(device, wait_count, waits, signal_count, signals);
}

static struct vk_sync_type
constanos_sync_type(void)
{
   return (struct vk_sync_type){
      .size = sizeof(struct nvkmd_constanos_sync),
      .features = VK_SYNC_FEATURE_BINARY |
                  VK_SYNC_FEATURE_TIMELINE |
                  VK_SYNC_FEATURE_GPU_WAIT |
                  VK_SYNC_FEATURE_GPU_MULTI_WAIT |
                  VK_SYNC_FEATURE_CPU_WAIT |
                  VK_SYNC_FEATURE_CPU_RESET |
                  VK_SYNC_FEATURE_CPU_SIGNAL |
                  VK_SYNC_FEATURE_WAIT_ANY |
                  VK_SYNC_FEATURE_WAIT_PENDING,
      .init = constanos_sync_init,
      .finish = constanos_sync_finish,
      .signal = constanos_sync_signal,
      .get_value = constanos_sync_get_value,
      .reset = constanos_sync_reset,
      .move = constanos_sync_move,
      .wait_many = constanos_sync_wait_many,
   };
}

/* ---- pdev ----------------------------------------------------------------------------------------------------------------- */

static void
fill_dev_info(struct nv_device_info *out, const struct nvg_info *in)
{
   memset(out, 0, sizeof(*out));
   out->type = NV_DEVICE_TYPE_DIS;
   out->device_id = in->device_id;
   out->chipset = in->chipset;
   strncpy(out->device_name, in->device_name, sizeof(out->device_name) - 1);
   strncpy(out->chipset_name, in->chipset_name, sizeof(out->chipset_name) - 1);
   out->sm = in->sm;
   out->gpc_count = in->gpc_count;
   out->tpc_count = in->tpc_count;
   out->mp_per_tpc = in->mp_per_tpc;
   out->max_warps_per_mp = in->max_warps_per_mp;
   out->max_blocks_per_mp = in->max_blocks_per_mp;
   out->has_transfer_queue = (in->cls_copy != 0);
   out->has_video = false;
   out->cls_copy = in->cls_copy;
   out->cls_eng2d = in->cls_eng2d;
   out->cls_eng3d = in->cls_eng3d;
   out->cls_m2mf = in->cls_m2mf;
   out->cls_compute = in->cls_compute;
   out->cls_gpfifo = in->cls_gpfifo;
   out->cls_vdec = in->cls_vdec;
   out->vram_size_B = in->vram_size_B;
   out->bar_size_B = in->bar_size_B;
   out->max_smem_per_wg_kB = in->max_smem_per_wg_kB;
   /* Ampere GA10x: the splits of the 128 KiB of L1/shared memory per SM. */
   static const uint16_t smem_sizes_kB[] = { 0, 8, 16, 32, 64, 100 };
   memcpy(out->sm_smem_sizes_kB, smem_sizes_kB, sizeof(smem_sizes_kB));
   out->sm_smem_size_count = ARRAY_SIZE(smem_sizes_kB);
   out->nc_atom_size_B = 64;
}

VkResult
nvkmd_constanos_try_create_pdev(struct vk_object_base *log_obj, enum nvk_debug debug_flags, struct nvkmd_pdev **pdev_out)
{
   const int fd = open(NVKMD_CONSTANOS_DEVICE_PATH, O_RDWR | O_CLOEXEC);
   if (fd < 0) {
      if (getenv("NVK_CONSTANOS_DEBUG"))
         fprintf(stderr, "nvkmd_constanos: open(%s): %s\n", NVKMD_CONSTANOS_DEVICE_PATH, strerror(errno));
      if (errno == ENOENT || errno == ENODEV)
         return VK_ERROR_INCOMPATIBLE_DRIVER;
      return vk_errorf(log_obj, VK_ERROR_INITIALIZATION_FAILED, "cannot open %s: %s (is another process using the GPU?)",
                       NVKMD_CONSTANOS_DEVICE_PATH, strerror(errno));
   }

   struct nvg_info info;
   const int r = constanos_ioctl(fd, NVG_IOC_INFO, &info);
   close(fd);
   if (r != 0 || info.abi_version != NVG_ABI_VERSION)
      return vk_errorf(log_obj, VK_ERROR_INCOMPATIBLE_DRIVER, "%s speaks ABI %u, not %u", NVKMD_CONSTANOS_DEVICE_PATH,
                       info.abi_version, NVG_ABI_VERSION);

   struct nvkmd_constanos_pdev *pdev = CALLOC_STRUCT(nvkmd_constanos_pdev);
   if (pdev == NULL)
      return vk_error(log_obj, VK_ERROR_OUT_OF_HOST_MEMORY);

   pdev->base.ops = &nvkmd_constanos_pdev_ops;
   pdev->base.debug_flags = debug_flags;
   fill_dev_info(&pdev->base.dev_info, &info);
   pdev->base.kmd_info = (struct nvkmd_info){
      .has_dma_buf = false,
      .has_get_vram_used = false,
      .has_alloc_tiled = false,
      .has_map_fixed = false,
      .has_overmap = false,
      .has_compression = false,
   };
   pdev->base.bind_align_B = 4096;

   pdev->sync_type = constanos_sync_type();
   pdev->sync_types[0] = &pdev->sync_type;
   pdev->sync_types[1] = NULL;
   pdev->base.sync_types = pdev->sync_types;

   *pdev_out = &pdev->base;
   return VK_SUCCESS;
}

static void
constanos_pdev_destroy(struct nvkmd_pdev *_pdev)
{
   FREE(nvkmd_constanos_pdev(_pdev));
}

static uint64_t
constanos_pdev_get_vram_used(struct nvkmd_pdev *pdev)
{
   return 0;
}

static int
constanos_pdev_get_drm_primary_fd(struct nvkmd_pdev *pdev)
{
   return -1;
}

const struct nvkmd_pdev_ops nvkmd_constanos_pdev_ops = {
   .destroy = constanos_pdev_destroy,
   .get_vram_used = constanos_pdev_get_vram_used,
   .get_drm_primary_fd = constanos_pdev_get_drm_primary_fd,
   .create_dev = nvkmd_constanos_create_dev,
};

/* ---- dev ------------------------------------------------------------------------------------------------------------------ */

VkResult
nvkmd_constanos_create_dev(struct nvkmd_pdev *pdev, struct vk_object_base *log_obj, struct nvkmd_dev **dev_out)
{
   struct nvkmd_constanos_dev *dev = CALLOC_STRUCT(nvkmd_constanos_dev);
   if (dev == NULL)
      return vk_error(log_obj, VK_ERROR_OUT_OF_HOST_MEMORY);

   dev->fd = open(NVKMD_CONSTANOS_DEVICE_PATH, O_RDWR | O_CLOEXEC);
   if (dev->fd < 0) {
      FREE(dev);
      return vk_errorf(log_obj, VK_ERROR_INITIALIZATION_FAILED, "cannot open %s: %s", NVKMD_CONSTANOS_DEVICE_PATH,
                       strerror(errno));
   }

   struct nvg_info info;
   if (constanos_ioctl(dev->fd, NVG_IOC_INFO, &info) != 0) {
      close(dev->fd);
      FREE(dev);
      return vk_error(log_obj, VK_ERROR_INITIALIZATION_FAILED);
   }

   /* One reservation for the whole range; sub-allocation and the replay split are ours. */
   struct nvg_va_alloc all = { .size = info.va_end - info.va_start, .align = 4096 };
   if (constanos_ioctl(dev->fd, NVG_IOC_VA_ALLOC, &all) != 0 || all.va != info.va_start) {
      close(dev->fd);
      FREE(dev);
      return vk_error(log_obj, VK_ERROR_INITIALIZATION_FAILED);
   }

   dev->base.ops = &nvkmd_constanos_dev_ops;
   dev->base.pdev = pdev;
   dev->base.va_start = info.va_start;
   dev->base.va_end = info.va_end;
   list_inithead(&dev->base.mems);
   simple_mtx_init(&dev->base.mems_mutex, mtx_plain);

   const uint64_t replay_start = info.va_end - (info.va_end - info.va_start) / REPLAY_HEAP_FRACTION;
   simple_mtx_init(&dev->heap_mutex, mtx_plain);
   util_vma_heap_init(&dev->heap, info.va_start, replay_start - info.va_start);
   util_vma_heap_init(&dev->replay_heap, replay_start, info.va_end - replay_start);

   *dev_out = &dev->base;
   return VK_SUCCESS;
}

static void
constanos_dev_destroy(struct nvkmd_dev *_dev)
{
   struct nvkmd_constanos_dev *dev = nvkmd_constanos_dev(_dev);

   util_vma_heap_finish(&dev->heap);
   util_vma_heap_finish(&dev->replay_heap);
   simple_mtx_destroy(&dev->heap_mutex);
   /* Closing the session unbinds and releases whatever is left. */
   close(dev->fd);
   FREE(dev);
}

static uint64_t
constanos_dev_get_gpu_timestamp(struct nvkmd_dev *_dev)
{
   struct nvg_timestamp t = { 0 };
   constanos_ioctl(nvkmd_constanos_dev(_dev)->fd, NVG_IOC_TIMESTAMP, &t);
   return t.ns;
}

static int
constanos_dev_get_drm_fd(struct nvkmd_dev *dev)
{
   return -1;
}

/* ---- va ------------------------------------------------------------------------------------------------------------------- */

static VkResult
alloc_heap_addr(struct nvkmd_constanos_dev *dev, struct vk_object_base *log_obj, enum nvkmd_va_flags flags, uint64_t size_B,
                uint64_t align_B, uint64_t fixed_addr, uint64_t *addr_out)
{
   VkResult result = VK_SUCCESS;
   simple_mtx_lock(&dev->heap_mutex);

   if (flags & NVKMD_VA_ALLOC_FIXED) {
      if (!(flags & NVKMD_VA_REPLAY) || (fixed_addr & (align_B - 1))) {
         result = vk_errorf(log_obj, VK_ERROR_INVALID_OPAQUE_CAPTURE_ADDRESS, "Bad capture address 0x%" PRIx64, fixed_addr);
      } else if (!util_vma_heap_alloc_addr(&dev->replay_heap, fixed_addr, size_B)) {
         result = vk_errorf(log_obj, VK_ERROR_INVALID_OPAQUE_CAPTURE_ADDRESS, "Replay address collision: 0x%" PRIx64, fixed_addr);
      } else {
         *addr_out = fixed_addr;
      }
   } else {
      struct util_vma_heap *heap = (flags & NVKMD_VA_REPLAY) ? &dev->replay_heap : &dev->heap;
      *addr_out = util_vma_heap_alloc(heap, size_B, align_B);
      if (*addr_out == 0)
         result = vk_errorf(log_obj, VK_ERROR_OUT_OF_DEVICE_MEMORY, "Failed to allocate virtual address range");
   }

   simple_mtx_unlock(&dev->heap_mutex);
   return result;
}

static void
free_heap_addr(struct nvkmd_constanos_dev *dev, enum nvkmd_va_flags flags, uint64_t addr, uint64_t size_B)
{
   simple_mtx_lock(&dev->heap_mutex);
   util_vma_heap_free((flags & NVKMD_VA_REPLAY) ? &dev->replay_heap : &dev->heap, addr, size_B);
   simple_mtx_unlock(&dev->heap_mutex);
}

/* Sparse ranges (soft faults) are not supported by the kernel yet: the range is reserved but unbound pages fault. */
VkResult
nvkmd_constanos_alloc_va(struct nvkmd_dev *_dev, struct vk_object_base *log_obj, enum nvkmd_va_flags flags, uint8_t pte_kind,
                         uint64_t size_B, uint64_t align_B, uint64_t fixed_addr, struct nvkmd_va **va_out)
{
   struct nvkmd_constanos_dev *dev = nvkmd_constanos_dev(_dev);

   struct nvkmd_constanos_va *va = CALLOC_STRUCT(nvkmd_constanos_va);
   if (va == NULL)
      return vk_error(log_obj, VK_ERROR_OUT_OF_HOST_MEMORY);

   assert(util_is_power_of_two_or_zero64(align_B));
   align_B = MAX2(align_B, _dev->pdev->bind_align_B);
   size_B = align64(size_B, align_B);

   assert((fixed_addr == 0) == !(flags & NVKMD_VA_ALLOC_FIXED));
   VkResult result = alloc_heap_addr(dev, log_obj, flags, size_B, align_B, fixed_addr, &va->base.addr);
   if (result != VK_SUCCESS) {
      FREE(va);
      return result;
   }

   va->base.ops = &nvkmd_constanos_va_ops;
   va->base.dev = &dev->base;
   va->base.flags = flags;
   va->base.pte_kind = pte_kind;
   va->base.size_B = size_B;

   *va_out = &va->base;
   return VK_SUCCESS;
}

static void
constanos_va_free(struct nvkmd_va *_va)
{
   struct nvkmd_constanos_dev *dev = nvkmd_constanos_dev(_va->dev);
   struct nvkmd_constanos_va *va = nvkmd_constanos_va(_va);

   struct nvg_va_unbind u = { .va = va->base.addr, .size = va->base.size_B };
   const int r = constanos_ioctl(dev->fd, NVG_IOC_VA_UNBIND, &u);

   /* If unbinding fails, the range is leaked rather than handed out again with pages still mapped. */
   if (r == 0)
      free_heap_addr(dev, va->base.flags, va->base.addr, va->base.size_B);
   FREE(va);
}

static VkResult
constanos_va_bind_mem(struct nvkmd_va *_va, struct vk_object_base *log_obj, uint64_t va_offset_B, struct nvkmd_mem *_mem,
                      uint64_t mem_offset_B, uint64_t range_B)
{
   struct nvkmd_constanos_dev *dev = nvkmd_constanos_dev(_va->dev);
   struct nvkmd_constanos_va *va = nvkmd_constanos_va(_va);
   struct nvkmd_constanos_mem *mem = nvkmd_constanos_mem(_mem);
   assert(_mem->dev == _va->dev);

   struct nvg_va_bind b = {
      .va = va->base.addr + va_offset_B,
      .size = range_B,
      .bo_offset = mem_offset_B,
      .handle = mem->handle,
      .pte_kind = va->base.pte_kind,
   };
   const int r = constanos_ioctl(dev->fd, NVG_IOC_VA_BIND, &b);
   if (r != 0)
      return vk_errorf(log_obj, VK_ERROR_UNKNOWN, "VA_BIND failed: %s", strerror(-r));
   return VK_SUCCESS;
}

static VkResult
constanos_va_unbind(struct nvkmd_va *_va, struct vk_object_base *log_obj, uint64_t va_offset_B, uint64_t range_B)
{
   struct nvkmd_constanos_dev *dev = nvkmd_constanos_dev(_va->dev);
   struct nvkmd_constanos_va *va = nvkmd_constanos_va(_va);

   struct nvg_va_unbind u = { .va = va->base.addr + va_offset_B, .size = range_B };
   const int r = constanos_ioctl(dev->fd, NVG_IOC_VA_UNBIND, &u);
   if (r != 0)
      return vk_errorf(log_obj, VK_ERROR_UNKNOWN, "VA_UNBIND failed: %s", strerror(-r));
   return VK_SUCCESS;
}

const struct nvkmd_va_ops nvkmd_constanos_va_ops = {
   .free = constanos_va_free,
   .bind_mem = constanos_va_bind_mem,
   .unbind = constanos_va_unbind,
};

/* ---- mem ------------------------------------------------------------------------------------------------------------------ */

static VkResult
constanos_alloc_mem(struct nvkmd_dev *_dev, struct vk_object_base *log_obj, uint64_t size_B, uint64_t align_B,
                    enum nvkmd_mem_flags flags, struct nvkmd_mem **mem_out)
{
   struct nvkmd_constanos_dev *dev = nvkmd_constanos_dev(_dev);
   const struct nv_device_info *info = &_dev->pdev->dev_info;

   /* Only one placement flag may be specified */
   assert(util_bitcount(flags & NVKMD_MEM_PLACEMENT_FLAGS) == 1);

   /* Anything the CPU may map lives in system memory (see the file comment); the rest of "local" is VRAM. */
   uint32_t bo_flags = NVG_BO_SYSTEM;
   if (!(flags & NVKMD_MEM_CAN_MAP) && !(flags & NVKMD_MEM_GART) && info->vram_size_B > 0)
      bo_flags = NVG_BO_VRAM;

   assert(util_is_power_of_two_or_zero64(align_B));
   align_B = MAX2(align_B, _dev->pdev->bind_align_B);
   size_B = align64(size_B, align_B);

   struct nvg_bo_create c = { .size = size_B, .flags = bo_flags };
   const int r = constanos_ioctl(dev->fd, NVG_IOC_BO_CREATE, &c);
   if (r != 0)
      return vk_errorf(log_obj, VK_ERROR_OUT_OF_DEVICE_MEMORY, "BO_CREATE failed: %s", strerror(-r));

   struct nvkmd_constanos_mem *mem = CALLOC_STRUCT(nvkmd_constanos_mem);
   if (mem == NULL) {
      struct nvg_bo_free f = { .handle = c.handle };
      constanos_ioctl(dev->fd, NVG_IOC_BO_FREE, &f);
      return vk_error(log_obj, VK_ERROR_OUT_OF_HOST_MEMORY);
   }

   /* Discrete-GPU maps are cached and coherent (the GPU snoops the CPU caches across PCIe). */
   flags |= NVKMD_MEM_COHERENT;

   nvkmd_mem_init(&dev->base, &mem->base, &nvkmd_constanos_mem_ops, flags, c.size_out, align_B);
   mem->handle = c.handle;
   mem->mmap_offset = c.mmap_offset;

   VkResult result = nvkmd_dev_alloc_va(&dev->base, log_obj, bo_flags == NVG_BO_SYSTEM ? NVKMD_VA_GART : 0,
                                        0 /* pte_kind */, c.size_out, align_B, 0 /* fixed_addr */, &mem->base.va);
   if (result != VK_SUCCESS)
      goto fail_mem;

   result = nvkmd_va_bind_mem(mem->base.va, log_obj, 0, &mem->base, 0, c.size_out);
   if (result != VK_SUCCESS)
      goto fail_va;

   *mem_out = &mem->base;
   return VK_SUCCESS;

fail_va:
   nvkmd_va_free(mem->base.va);
fail_mem: {
   struct nvg_bo_free f = { .handle = c.handle };
   constanos_ioctl(dev->fd, NVG_IOC_BO_FREE, &f);
   FREE(mem);
   return result;
}
}

static VkResult
constanos_alloc_tiled_mem(struct nvkmd_dev *dev, struct vk_object_base *log_obj, uint64_t size_B, uint64_t align_B,
                          uint8_t pte_kind, uint16_t tile_mode, enum nvkmd_mem_flags flags, struct nvkmd_mem **mem_out)
{
   return vk_error(log_obj, VK_ERROR_FEATURE_NOT_PRESENT);
}

static VkResult
constanos_import_dma_buf(struct nvkmd_dev *dev, struct vk_object_base *log_obj, int fd, struct nvkmd_mem **mem_out)
{
   return vk_error(log_obj, VK_ERROR_INVALID_EXTERNAL_HANDLE);
}

static void
constanos_mem_free(struct nvkmd_mem *_mem)
{
   struct nvkmd_constanos_mem *mem = nvkmd_constanos_mem(_mem);
   struct nvkmd_constanos_dev *dev = nvkmd_constanos_dev(_mem->dev);

   /* Unbinding first, then the BO: the kernel keeps the storage while a range is bound, but the GPU must not see it any more. */
   nvkmd_va_free(mem->base.va);
   struct nvg_bo_free f = { .handle = mem->handle };
   ASSERTED int r = constanos_ioctl(dev->fd, NVG_IOC_BO_FREE, &f);
   assert(r == 0);
   FREE(mem);
}

static VkResult
constanos_mem_map(struct nvkmd_mem *_mem, struct vk_object_base *log_obj, enum nvkmd_mem_map_flags map_flags, void *fixed_addr,
                  void **map_out)
{
   struct nvkmd_constanos_mem *mem = nvkmd_constanos_mem(_mem);
   struct nvkmd_constanos_dev *dev = nvkmd_constanos_dev(_mem->dev);

   if (mem->mmap_offset == ~0ull)
      return vk_errorf(log_obj, VK_ERROR_MEMORY_MAP_FAILED, "device-local memory cannot be mapped by the CPU");

   int prot = 0;
   if (map_flags & NVKMD_MEM_MAP_RD)
      prot |= PROT_READ;
   if (map_flags & NVKMD_MEM_MAP_WR)
      prot |= PROT_WRITE;

   int flags = MAP_SHARED;
   if (map_flags & NVKMD_MEM_MAP_FIXED)
      flags |= MAP_FIXED;

   void *map = mmap(fixed_addr, mem->base.size_B, prot, flags, dev->fd, mem->mmap_offset);
   if (map == MAP_FAILED)
      return vk_error(log_obj, VK_ERROR_MEMORY_MAP_FAILED);

   *map_out = map;
   return VK_SUCCESS;
}

static void
constanos_mem_unmap(struct nvkmd_mem *_mem, enum nvkmd_mem_map_flags flags, void *map)
{
   munmap(map, _mem->size_B);
}

static uint32_t
constanos_mem_log_handle(struct nvkmd_mem *_mem)
{
   return nvkmd_constanos_mem(_mem)->handle;
}

const struct nvkmd_mem_ops nvkmd_constanos_mem_ops = {
   .free = constanos_mem_free,
   .map = constanos_mem_map,
   .unmap = constanos_mem_unmap,
   .log_handle = constanos_mem_log_handle,
};

const struct nvkmd_dev_ops nvkmd_constanos_dev_ops = {
   .destroy = constanos_dev_destroy,
   .get_gpu_timestamp = constanos_dev_get_gpu_timestamp,
   .get_drm_fd = constanos_dev_get_drm_fd,
   .alloc_mem = constanos_alloc_mem,
   .alloc_tiled_mem = constanos_alloc_tiled_mem,
   .import_dma_buf = constanos_import_dma_buf,
   .alloc_va = nvkmd_constanos_alloc_va,
   .create_ctx = nvkmd_constanos_create_ctx,
};

/* ---- ctx: execution ------------------------------------------------------------------------------------------------------- */

static inline void
add_ref(struct nvg_sync_ref *refs, uint32_t *count, const struct vk_sync *sync, uint64_t value)
{
   refs[(*count)++] = (struct nvg_sync_ref){ .handle = to_sync((struct vk_sync *)sync)->handle, .value = sync_value(sync, value) };
}

static VkResult
constanos_exec_ctx_flush(struct nvkmd_ctx *_ctx, struct vk_object_base *log_obj)
{
   struct nvkmd_constanos_exec_ctx *ctx = nvkmd_constanos_exec_ctx(_ctx);

   if (ctx->push_count == 0 && ctx->wait_count == 0 && ctx->sig_count == 0)
      return VK_SUCCESS;

   struct nvg_exec e = {
      .ctx = ctx->ctx,
      .push_count = ctx->push_count,
      .wait_count = ctx->wait_count,
      .sig_count = ctx->sig_count,
      .pushes = (uintptr_t)ctx->pushes,
      .waits = (uintptr_t)ctx->waits,
      .signals = (uintptr_t)ctx->sigs,
   };

   for (;;) {
      const int r = constanos_ioctl(ctx->fd, NVG_IOC_EXEC, &e);
      if (r == 0)
         break;
      if (r == -EAGAIN) {
         /* A wait is not satisfied yet; nothing was queued. */
         const int w = wait_refs(ctx->fd, ctx->waits, ctx->wait_count, 0, UINT64_MAX);
         if (w != 0)
            return vk_errorf(log_obj, VK_ERROR_UNKNOWN, "waiting for an EXEC's dependencies failed: %s", strerror(-w));
         continue;
      }
      return vk_errorf(log_obj, r == -ENODEV ? VK_ERROR_DEVICE_LOST : VK_ERROR_UNKNOWN, "NVG_IOC_EXEC failed: %s", strerror(-r));
   }

   ctx->push_count = 0;
   ctx->wait_count = 0;
   ctx->sig_count = 0;
   return VK_SUCCESS;
}

static VkResult
constanos_exec_ctx_wait(struct nvkmd_ctx *_ctx, struct vk_object_base *log_obj, uint32_t wait_count,
                        const struct vk_sync_wait *waits)
{
   struct nvkmd_constanos_exec_ctx *ctx = nvkmd_constanos_exec_ctx(_ctx);

   for (uint32_t i = 0; i < wait_count; i++) {
      if (unlikely(ctx->wait_count >= NVKMD_CONSTANOS_MAX_SYNCS)) {
         VkResult result = constanos_exec_ctx_flush(_ctx, log_obj);
         if (result != VK_SUCCESS)
            return result;
      }
      add_ref(ctx->waits, &ctx->wait_count, waits[i].sync, waits[i].wait_value);
   }
   return VK_SUCCESS;
}

static VkResult
constanos_exec_ctx_exec(struct nvkmd_ctx *_ctx, struct vk_object_base *log_obj, uint32_t exec_count,
                        const struct nvkmd_ctx_exec *execs)
{
   struct nvkmd_constanos_exec_ctx *ctx = nvkmd_constanos_exec_ctx(_ctx);

   for (uint32_t i = 0; i < exec_count; i++) {
      /* A push that ends in an incomplete method needs the next one in the same submit: keep the run together. */
      uint32_t incomplete_count = 0;
      for (uint32_t j = i; j < exec_count; j++) {
         if (!execs[j].incomplete)
            break;
         assert(j < exec_count - 1);
         incomplete_count++;
      }
      assert(incomplete_count < NVKMD_CONSTANOS_MAX_PUSHES);

      if (unlikely(ctx->push_count + incomplete_count >= NVKMD_CONSTANOS_MAX_PUSHES)) {
         VkResult result = constanos_exec_ctx_flush(_ctx, log_obj);
         if (result != VK_SUCCESS)
            return result;
      }

      /* The hardware limit on all current GPUs */
      assert((execs[i].addr % 4) == 0 && (execs[i].size_B % 4) == 0);
      assert(execs[i].size_B < (1u << 23));

      ctx->pushes[ctx->push_count++] = (struct nvg_push){
         .va = execs[i].addr,
         .bytes = execs[i].size_B,
         .flags = execs[i].no_prefetch ? NVG_PUSH_NO_PREFETCH : 0,
      };
   }
   return VK_SUCCESS;
}

static VkResult
constanos_exec_ctx_signal(struct nvkmd_ctx *_ctx, struct vk_object_base *log_obj, uint32_t signal_count,
                          const struct vk_sync_signal *signals)
{
   struct nvkmd_constanos_exec_ctx *ctx = nvkmd_constanos_exec_ctx(_ctx);

   for (uint32_t i = 0; i < signal_count; i++) {
      if (unlikely(ctx->sig_count >= NVKMD_CONSTANOS_MAX_SYNCS)) {
         VkResult result = constanos_exec_ctx_flush(_ctx, log_obj);
         if (result != VK_SUCCESS)
            return result;
      }
      add_ref(ctx->sigs, &ctx->sig_count, signals[i].sync, signals[i].signal_value);
   }
   return constanos_exec_ctx_flush(_ctx, log_obj);
}

static VkResult
constanos_exec_ctx_sync(struct nvkmd_ctx *_ctx, struct vk_object_base *log_obj)
{
   struct nvkmd_constanos_exec_ctx *ctx = nvkmd_constanos_exec_ctx(_ctx);

   if (unlikely(ctx->sig_count >= NVKMD_CONSTANOS_MAX_SYNCS)) {
      VkResult result = constanos_exec_ctx_flush(_ctx, log_obj);
      if (result != VK_SUCCESS)
         return result;
   }

   const uint64_t value = ++ctx->sync_value;
   ctx->sigs[ctx->sig_count++] = (struct nvg_sync_ref){ .handle = ctx->sync, .value = value };
   VkResult result = constanos_exec_ctx_flush(_ctx, log_obj);
   if (result != VK_SUCCESS)
      return result;

   struct nvg_sync_ref done = { .handle = ctx->sync, .value = value };
   const int r = wait_refs(ctx->fd, &done, 1, 0, UINT64_MAX);
   if (r != 0)
      return vk_errorf(log_obj, VK_ERROR_DEVICE_LOST, "waiting for the context to go idle failed: %s", strerror(-r));
   return VK_SUCCESS;
}

static void
constanos_exec_ctx_destroy(struct nvkmd_ctx *_ctx)
{
   struct nvkmd_constanos_exec_ctx *ctx = nvkmd_constanos_exec_ctx(_ctx);

   struct nvg_sync_destroy sd = { .handle = ctx->sync };
   constanos_ioctl(ctx->fd, NVG_IOC_SYNC_DESTROY, &sd);
   struct nvg_ctx_destroy cd = { .ctx = ctx->ctx };
   constanos_ioctl(ctx->fd, NVG_IOC_CTX_DESTROY, &cd);
   FREE(ctx);
}

const struct nvkmd_ctx_ops nvkmd_constanos_exec_ctx_ops = {
   .destroy = constanos_exec_ctx_destroy,
   .wait = constanos_exec_ctx_wait,
   .exec = constanos_exec_ctx_exec,
   .signal = constanos_exec_ctx_signal,
   .flush = constanos_exec_ctx_flush,
   .sync = constanos_exec_ctx_sync,
};

/* ---- ctx: binds ----------------------------------------------------------------------------------------------------------- */

static VkResult
constanos_bind_ctx_wait(struct nvkmd_ctx *_ctx, struct vk_object_base *log_obj, uint32_t wait_count,
                        const struct vk_sync_wait *waits)
{
   struct nvkmd_constanos_bind_ctx *ctx = nvkmd_constanos_bind_ctx(_ctx);

   for (uint32_t i = 0; i < wait_count; i++) {
      if (unlikely(ctx->wait_count >= NVKMD_CONSTANOS_MAX_SYNCS)) {
         /* Binds are synchronous, so waiting now is the same as waiting later. */
         const int r = wait_refs(ctx->fd, ctx->waits, ctx->wait_count, 0, UINT64_MAX);
         if (r != 0)
            return vk_errorf(log_obj, VK_ERROR_UNKNOWN, "bind wait failed: %s", strerror(-r));
         ctx->wait_count = 0;
      }
      add_ref(ctx->waits, &ctx->wait_count, waits[i].sync, waits[i].wait_value);
   }
   return VK_SUCCESS;
}

static VkResult
bind_ctx_settle_waits(struct nvkmd_constanos_bind_ctx *ctx, struct vk_object_base *log_obj)
{
   if (ctx->wait_count == 0)
      return VK_SUCCESS;
   const int r = wait_refs(ctx->fd, ctx->waits, ctx->wait_count, 0, UINT64_MAX);
   if (r != 0)
      return vk_errorf(log_obj, VK_ERROR_UNKNOWN, "bind wait failed: %s", strerror(-r));
   ctx->wait_count = 0;
   return VK_SUCCESS;
}

static VkResult
constanos_bind_ctx_bind(struct nvkmd_ctx *_ctx, struct vk_object_base *log_obj, uint32_t bind_count,
                        const struct nvkmd_ctx_bind *binds)
{
   struct nvkmd_constanos_bind_ctx *ctx = nvkmd_constanos_bind_ctx(_ctx);

   VkResult result = bind_ctx_settle_waits(ctx, log_obj);
   if (result != VK_SUCCESS)
      return result;

   for (uint32_t i = 0; i < bind_count; i++) {
      const uint64_t addr = binds[i].va->addr + binds[i].va_offset_B;
      int r;
      if (binds[i].op == NVKMD_BIND_OP_BIND) {
         struct nvg_va_bind b = {
            .va = addr,
            .size = binds[i].range_B,
            .bo_offset = binds[i].mem_offset_B,
            .handle = nvkmd_constanos_mem(binds[i].mem)->handle,
            .pte_kind = binds[i].va->pte_kind,
         };
         r = constanos_ioctl(ctx->fd, NVG_IOC_VA_BIND, &b);
      } else {
         struct nvg_va_unbind u = { .va = addr, .size = binds[i].range_B };
         r = constanos_ioctl(ctx->fd, NVG_IOC_VA_UNBIND, &u);
      }
      if (r != 0)
         return vk_errorf(log_obj, VK_ERROR_UNKNOWN, "sparse bind failed: %s", strerror(-r));
   }
   return VK_SUCCESS;
}

static VkResult
constanos_bind_ctx_signal(struct nvkmd_ctx *_ctx, struct vk_object_base *log_obj, uint32_t signal_count,
                          const struct vk_sync_signal *signals)
{
   struct nvkmd_constanos_bind_ctx *ctx = nvkmd_constanos_bind_ctx(_ctx);

   /* Everything this context did is already done. */
   VkResult result = bind_ctx_settle_waits(ctx, log_obj);
   if (result != VK_SUCCESS)
      return result;

   for (uint32_t i = 0; i < signal_count; i++) {
      struct nvg_sync_signal s = {
         .handle = to_sync(signals[i].sync)->handle,
         .value = sync_value(signals[i].sync, signals[i].signal_value),
      };
      const int r = constanos_ioctl(ctx->fd, NVG_IOC_SYNC_SIGNAL, &s);
      if (r != 0)
         return vk_errorf(log_obj, VK_ERROR_UNKNOWN, "signal failed: %s", strerror(-r));
   }
   return VK_SUCCESS;
}

static VkResult
constanos_bind_ctx_flush(struct nvkmd_ctx *_ctx, struct vk_object_base *log_obj)
{
   return bind_ctx_settle_waits(nvkmd_constanos_bind_ctx(_ctx), log_obj);
}

static VkResult
constanos_bind_ctx_sync(struct nvkmd_ctx *_ctx, struct vk_object_base *log_obj)
{
   return bind_ctx_settle_waits(nvkmd_constanos_bind_ctx(_ctx), log_obj);
}

static void
constanos_bind_ctx_destroy(struct nvkmd_ctx *_ctx)
{
   FREE(nvkmd_constanos_bind_ctx(_ctx));
}

const struct nvkmd_ctx_ops nvkmd_constanos_bind_ctx_ops = {
   .destroy = constanos_bind_ctx_destroy,
   .wait = constanos_bind_ctx_wait,
   .bind = constanos_bind_ctx_bind,
   .signal = constanos_bind_ctx_signal,
   .flush = constanos_bind_ctx_flush,
   .sync = constanos_bind_ctx_sync,
};

/* ---- ctx: creation -------------------------------------------------------------------------------------------------------- */

VkResult
nvkmd_constanos_create_ctx(struct nvkmd_dev *_dev, struct vk_object_base *log_obj, enum nvkmd_engines engines,
                           struct nvkmd_ctx **ctx_out)
{
   struct nvkmd_constanos_dev *dev = nvkmd_constanos_dev(_dev);

   if (engines & NVKMD_ENGINE_BIND) {
      assert(engines == NVKMD_ENGINE_BIND);
      struct nvkmd_constanos_bind_ctx *ctx = CALLOC_STRUCT(nvkmd_constanos_bind_ctx);
      if (ctx == NULL)
         return vk_error(log_obj, VK_ERROR_OUT_OF_HOST_MEMORY);
      ctx->base.ops = &nvkmd_constanos_bind_ctx_ops;
      ctx->base.dev = &dev->base;
      ctx->fd = dev->fd;
      *ctx_out = &ctx->base;
      return VK_SUCCESS;
   }

   STATIC_ASSERT(NVKMD_ENGINE_COPY == (int)NVG_ENGINE_COPY);
   STATIC_ASSERT(NVKMD_ENGINE_2D == (int)NVG_ENGINE_2D);
   STATIC_ASSERT(NVKMD_ENGINE_3D == (int)NVG_ENGINE_3D);
   STATIC_ASSERT(NVKMD_ENGINE_M2MF == (int)NVG_ENGINE_M2MF);
   STATIC_ASSERT(NVKMD_ENGINE_COMPUTE == (int)NVG_ENGINE_COMPUTE);

   struct nvkmd_constanos_exec_ctx *ctx = CALLOC_STRUCT(nvkmd_constanos_exec_ctx);
   if (ctx == NULL)
      return vk_error(log_obj, VK_ERROR_OUT_OF_HOST_MEMORY);

   /* VDEC is not something this device offers; everything else maps one to one. */
   struct nvg_ctx_create c = { .engines = engines & (NVG_ENGINE_COPY | NVG_ENGINE_2D | NVG_ENGINE_3D | NVG_ENGINE_M2MF | NVG_ENGINE_COMPUTE) };
   if (c.engines == 0) {
      FREE(ctx);
      return vk_error(log_obj, VK_ERROR_FEATURE_NOT_PRESENT);
   }

   const int r = constanos_ioctl(dev->fd, NVG_IOC_CTX_CREATE, &c);
   if (r != 0) {
      FREE(ctx);
      return vk_error(log_obj, r == -ENOSPC ? VK_ERROR_TOO_MANY_OBJECTS : VK_ERROR_OUT_OF_HOST_MEMORY);
   }

   struct nvg_sync_create sc = { .initial = 0 };
   if (constanos_ioctl(dev->fd, NVG_IOC_SYNC_CREATE, &sc) != 0) {
      struct nvg_ctx_destroy cd = { .ctx = c.ctx };
      constanos_ioctl(dev->fd, NVG_IOC_CTX_DESTROY, &cd);
      FREE(ctx);
      return vk_error(log_obj, VK_ERROR_OUT_OF_HOST_MEMORY);
   }

   ctx->base.ops = &nvkmd_constanos_exec_ctx_ops;
   ctx->base.dev = &dev->base;
   ctx->fd = dev->fd;
   ctx->ctx = c.ctx;
   ctx->sync = sc.handle;

   *ctx_out = &ctx->base;
   return VK_SUCCESS;
}
