// constanos_vk_window.h — a window for a Vulkan program on constanos (G5 layer 4, docs/gpu/g5-graphics-stack-plan.md).
//
// The program owns the window and its connection to the compositor, as a Wayland client does (it makes the socket and the surface with
// constanos_gui_wire.h and reads the events: keys, the pointer, `configure`, `close`). The WSI inside NVK, which the program links statically,
// only needs five things from it, the hooks below. The program makes a VkSurfaceKHR of them with nvk_constanos_surface_create() and then uses
// VK_KHR_swapchain as it would anywhere: vkAcquireNextImageKHR, vkQueuePresentKHR.
//
// What a swapchain does with the hooks: each image is a buffer of VRAM (rows `pitch_B` apart, bytes B, G, R, X: XRGB8888) exported as a
// descriptor and handed over with `send_buffer` (the compositor's `create_gpu_buffer`). A present waits for the GPU to finish the image, then
// `commit`s it (`attach` + `damage` + `commit`). The compositor reads the buffer where it is; the image is the program's again when the
// compositor says it let go (its `release`), which the program's event loop passes on with nvk_constanos_surface_buffer_released(). An acquire that
// finds every image held calls `pump` until one is released, so the program's event handling keeps running while it waits.
//
// Pure C, no dependencies: Mesa's WSI (src/vulkan/wsi/constanos_window.h is a copy) and the programs include it.

#ifndef CONSTANOS_VK_WINDOW_H
#define CONSTANOS_VK_WINDOW_H

#include <stdint.h>

struct constanos_window {
   void *user;

   /* A fresh object id of the connection, for a buffer (the program's own counter: ids are never reused). */
   uint32_t (*new_id)(void *user);

   /* Make buffer `id` of the descriptor `fd` (`size_B` bytes of GPU memory, `width` x `height` pixels, rows `pitch_B` bytes apart). The WSI
    * closes `fd` afterwards: send it (SCM_RIGHTS), do not keep it. 0, or a negative errno. */
   int (*send_buffer)(void *user, uint32_t id, int fd, uint64_t size_B, uint32_t width, uint32_t height, uint32_t pitch_B);

   /* The picture in buffer `id` is complete: attach it to the surface, damage all of it and commit. 0, or a negative errno. */
   int (*commit)(void *user, uint32_t id);

   /* The WSI is done with buffer `id` (the swapchain is going away): destroy it. */
   int (*destroy_buffer)(void *user, uint32_t id);

   /* Wait up to `timeout_ms` for the compositor to say something and handle every event that arrives (calling
    * nvk_constanos_surface_buffer_released for a `release`). 0 when it returned (events or not), a negative errno when the connection is gone. */
   int (*pump)(void *user, int timeout_ms);
};

#ifdef __cplusplus
extern "C" {
#endif

/* NVK is linked into the program, so these are plain functions (vulkan.h comes first for the Vk types). */
#ifdef VULKAN_H_
VkResult nvk_constanos_surface_create(VkInstance instance, const struct constanos_window *window, VkSurfaceKHR *surface);
/* Buffer `id` (as given to send_buffer) was released by the compositor. */
void nvk_constanos_surface_buffer_released(VkSurfaceKHR surface, uint32_t id);
#endif

#ifdef __cplusplus
}
#endif

#endif
