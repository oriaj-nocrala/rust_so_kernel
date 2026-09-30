/* Prints sizeof/offsetof of every struct in nvgpu/uapi/nvgpu.h, and the ioctl numbers. The values asserted in nvgpu/src/uapi.rs
 * are this program's output:
 *
 *     clang -I nvgpu/uapi -Wall -Wextra -o /tmp/uapi nvgpu/gen/uapi.c && /tmp/uapi
 */
#include <stddef.h>
#include <stdio.h>
#include "nvgpu.h"

#define S(t) printf("size %s %zu\n", #t, sizeof(struct t))
#define O(t, f) printf("off %s.%s %zu\n", #t, #f, offsetof(struct t, f))
#define I(n) printf("ioc %s 0x%08x\n", #n, (unsigned)n)

int main(void) {
   S(nvg_info); O(nvg_info, abi_version); O(nvg_info, flags); O(nvg_info, device_id); O(nvg_info, sm); O(nvg_info, gpc_count);
   O(nvg_info, tpc_count); O(nvg_info, mp_per_tpc); O(nvg_info, cls_copy); O(nvg_info, cls_compute); O(nvg_info, cls_vdec);
   O(nvg_info, max_smem_per_wg_kB); O(nvg_info, vram_size_B); O(nvg_info, vram_used_B); O(nvg_info, bar_size_B); O(nvg_info, va_start);
   O(nvg_info, va_end); O(nvg_info, device_name); O(nvg_info, chipset_name);
   S(nvg_bo_create); O(nvg_bo_create, flags); O(nvg_bo_create, handle); O(nvg_bo_create, mmap_offset); O(nvg_bo_create, size_out);
   S(nvg_bo_free);
   S(nvg_va_alloc); O(nvg_va_alloc, align); O(nvg_va_alloc, va); O(nvg_va_alloc, flags);
   S(nvg_va_free);
   S(nvg_va_bind); O(nvg_va_bind, size); O(nvg_va_bind, bo_offset); O(nvg_va_bind, handle); O(nvg_va_bind, pte_kind);
   S(nvg_va_unbind);
   S(nvg_ctx_create); S(nvg_ctx_destroy);
   S(nvg_push); O(nvg_push, bytes); O(nvg_push, flags);
   S(nvg_sync_ref); O(nvg_sync_ref, value);
   S(nvg_exec); O(nvg_exec, push_count); O(nvg_exec, wait_count); O(nvg_exec, sig_count); O(nvg_exec, pushes); O(nvg_exec, waits);
   O(nvg_exec, signals);
   S(nvg_sync_create); S(nvg_sync_destroy); S(nvg_sync_signal); O(nvg_sync_signal, value);
   S(nvg_sync_wait); O(nvg_sync_wait, count); O(nvg_sync_wait, flags); O(nvg_sync_wait, timeout_ns); O(nvg_sync_wait, first_ready);
   S(nvg_sync_query); O(nvg_sync_query, value);
   S(nvg_timestamp);
   I(NVG_IOC_INFO); I(NVG_IOC_BO_CREATE); I(NVG_IOC_BO_FREE); I(NVG_IOC_VA_ALLOC); I(NVG_IOC_VA_FREE); I(NVG_IOC_VA_BIND);
   I(NVG_IOC_VA_UNBIND); I(NVG_IOC_CTX_CREATE); I(NVG_IOC_CTX_DESTROY); I(NVG_IOC_EXEC); I(NVG_IOC_SYNC_CREATE);
   I(NVG_IOC_SYNC_DESTROY); I(NVG_IOC_SYNC_SIGNAL); I(NVG_IOC_SYNC_WAIT); I(NVG_IOC_SYNC_QUERY); I(NVG_IOC_TIMESTAMP);
   return 0;
}
