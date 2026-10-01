/*
 * vk_window: a Vulkan program in a window (G5 layer 4, slice 2; docs/gpu/g5-graphics-stack-plan.md). It owns its connection to the compositor
 * (constanos_gui_vk.h), makes a VkSurfaceKHR of it with nvk_constanos_surface_create and presents through VK_KHR_swapchain: NVK's WSI exports
 * every image as a GPU buffer, sends it with create_gpu_buffer, commits it when the GPU is done with it and gets it back when the compositor
 * releases it. Each frame is a clear to a colour that changes with the frame, so a compositor that reads the buffer can tell the frames apart.
 *
 *   VK_WINDOW_FRAMES=<n>   frames to present (default 60)
 *   VK_WINDOW_W/H=<n>      the window's size (default 640x360); halfway through the swapchain is made again at W+64 x H+32 (a resize)
 *   $GUI_DISPLAY           the compositor's socket
 */
#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <unwind.h>

#include "constanos_gui_vk.h"

static _Unwind_Reason_Code trace_cb(struct _Unwind_Context *c, void *arg) {
   (void)arg;
   printf("VK BT %#lx\n", (unsigned long)_Unwind_GetIP(c));
   return _URC_NO_REASON;
}

void __assert_fail(const char *expr, const char *file, int line, const char *func) {
   printf("VK ASSERT %s (%s: %s: %d)\n", expr, file, func, line);
   _Unwind_Backtrace(trace_cb, NULL);
   _exit(134);
}

void abort(void) {
   _Unwind_Backtrace(trace_cb, NULL);
   _exit(134);
}

extern PFN_vkVoidFunction vk_icdGetInstanceProcAddr(VkInstance instance, const char *name);

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("VK ok   %s\n", #cond); else { failures++; printf("VK FAIL %s (line %d): ", #cond, __LINE__); printf(__VA_ARGS__); printf("\n"); } } while (0)
#define VKOK(call) do { VkResult r_ = (call); if (r_ != VK_SUCCESS) { failures++; printf("VK FAIL %s -> %d (line %d)\n", #call, (int)r_, __LINE__); goto done; } } while (0)
#define GLOBAL(name) PFN_##name name = (PFN_##name)vk_icdGetInstanceProcAddr(NULL, #name)
#define INST(name) PFN_##name name = (PFN_##name)vk_icdGetInstanceProcAddr(instance, #name)
#define DEV(name) PFN_##name name = (PFN_##name)vkGetDeviceProcAddr(device, #name)

#define MAX_IMAGES 8

struct chain {
   VkSwapchainKHR sc;
   VkImage images[MAX_IMAGES];
   uint32_t count;
   uint32_t w, h;
};

int main(void) {
   setvbuf(stdout, NULL, _IONBF, 0);
   const char *e;
   const unsigned frames_wanted = (e = getenv("VK_WINDOW_FRAMES")) ? (unsigned)atoi(e) : 60;
   uint32_t W = (e = getenv("VK_WINDOW_W")) ? (uint32_t)atoi(e) : 640, H = (e = getenv("VK_WINDOW_H")) ? (uint32_t)atoi(e) : 360;

   VkInstance instance = VK_NULL_HANDLE;
   VkDevice device = VK_NULL_HANDLE;
   VkSurfaceKHR surface = VK_NULL_HANDLE;
   VkCommandPool cpool = VK_NULL_HANDLE;
   VkSemaphore acquire_sem = VK_NULL_HANDLE, render_sem[MAX_IMAGES] = { VK_NULL_HANDLE };
   VkFence fence = VK_NULL_HANDLE;
   struct chain cur = { VK_NULL_HANDLE }, old = { VK_NULL_HANDLE };
   struct gvk_window win;
   int have_window = 0;
   PFN_vkGetDeviceProcAddr vkGetDeviceProcAddr = NULL;

   GLOBAL(vkCreateInstance);
   if (!vkCreateInstance) return 1;
   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .pApplicationName = "vk_window", .apiVersion = VK_API_VERSION_1_3 };
   static const char *const instance_exts[] = { "VK_KHR_surface", "VK_EXT_headless_surface" };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app, .enabledExtensionCount = 2, .ppEnabledExtensionNames = instance_exts };
   VKOK(vkCreateInstance(&ici, NULL, &instance));
   INST(vkEnumeratePhysicalDevices);
   INST(vkGetPhysicalDeviceQueueFamilyProperties);
   INST(vkCreateDevice);
   INST(vkGetPhysicalDeviceSurfaceCapabilitiesKHR);
   INST(vkGetPhysicalDeviceSurfaceFormatsKHR);
   INST(vkGetPhysicalDeviceSurfacePresentModesKHR);
   INST(vkGetPhysicalDeviceSurfaceSupportKHR);
   INST(vkDestroySurfaceKHR);
   INST(vkDestroyInstance);
   vkGetDeviceProcAddr = (PFN_vkGetDeviceProcAddr)vk_icdGetInstanceProcAddr(instance, "vkGetDeviceProcAddr");

   uint32_t n = 1;
   VkPhysicalDevice pdev = VK_NULL_HANDLE;
   VkResult r = vkEnumeratePhysicalDevices(instance, &n, &pdev);
   CHECK((r == VK_SUCCESS || r == VK_INCOMPLETE) && pdev != VK_NULL_HANDLE, "a physical device (%d)", (int)r);
   if (!pdev) goto done;
   uint32_t nq = 16;
   VkQueueFamilyProperties qf[16];
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nq, qf);
   int family = -1;
   for (uint32_t i = 0; i < nq && family < 0; i++)
      if (qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) family = (int)i;
   CHECK(family >= 0, "a graphics queue family");
   if (family < 0) goto done;
   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = (uint32_t)family, .queueCount = 1, .pQueuePriorities = &prio };
   static const char *const device_exts[] = { "VK_KHR_swapchain" };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
                              .enabledExtensionCount = 1, .ppEnabledExtensionNames = device_exts };
   VKOK(vkCreateDevice(pdev, &dci, NULL, &device));
   DEV(vkGetDeviceQueue); DEV(vkCreateSwapchainKHR); DEV(vkDestroySwapchainKHR); DEV(vkGetSwapchainImagesKHR); DEV(vkAcquireNextImageKHR);
   DEV(vkQueuePresentKHR); DEV(vkCreateSemaphore); DEV(vkDestroySemaphore); DEV(vkCreateFence); DEV(vkDestroyFence); DEV(vkWaitForFences);
   DEV(vkResetFences); DEV(vkCreateCommandPool); DEV(vkDestroyCommandPool); DEV(vkAllocateCommandBuffers); DEV(vkResetCommandBuffer);
   DEV(vkBeginCommandBuffer); DEV(vkEndCommandBuffer); DEV(vkCmdPipelineBarrier); DEV(vkCmdClearColorImage); DEV(vkQueueSubmit);
   DEV(vkDeviceWaitIdle); DEV(vkDestroyDevice);
   VkQueue queue;
   vkGetDeviceQueue(device, (uint32_t)family, 0, &queue);

   // ---- the window: the connection is the program's, the surface is made of it
   if (gvk_open(&win, "vk_window") != 0) {
      printf("VK FAIL no compositor to connect to ($GUI_DISPLAY)\n");
      failures++;
      goto done;
   }
   have_window = 1;
   printf("VK window: the compositor suggests %dx%d\n", win.cfg_w, win.cfg_h);
   CHECK(gvk_surface_create(&win, instance, &surface) == 0, "the window's surface");
   VkBool32 supported = VK_FALSE;
   VKOK(vkGetPhysicalDeviceSurfaceSupportKHR(pdev, (uint32_t)family, surface, &supported));
   CHECK(supported, "the queue family can present to it");
   VkSurfaceCapabilitiesKHR caps;
   VKOK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(pdev, surface, &caps));
   CHECK(caps.currentExtent.width == 0xffffffffu && caps.minImageCount == 3, "a window has no size of its own (%u x %u, min images %u)", caps.currentExtent.width, caps.currentExtent.height, caps.minImageCount);
   uint32_t nf = 8;
   VkSurfaceFormatKHR formats[8];
   VKOK(vkGetPhysicalDeviceSurfaceFormatsKHR(pdev, surface, &nf, formats));
   CHECK(nf >= 1 && formats[0].format == VK_FORMAT_B8G8R8A8_UNORM, "B8G8R8A8 first (%u formats)", nf);
   uint32_t nm = 4;
   VkPresentModeKHR modes[4];
   VKOK(vkGetPhysicalDeviceSurfacePresentModesKHR(pdev, surface, &nm, modes));
   CHECK(nm == 1 && modes[0] == VK_PRESENT_MODE_FIFO_KHR, "FIFO only (%u modes)", nm);

   VkCommandPoolCreateInfo cpi = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT, .queueFamilyIndex = (uint32_t)family };
   VKOK(vkCreateCommandPool(device, &cpi, NULL, &cpool));
   VkCommandBufferAllocateInfo cbai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = cpool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
   VkCommandBuffer cb;
   VKOK(vkAllocateCommandBuffers(device, &cbai, &cb));
   VkSemaphoreCreateInfo semi = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO };
   VKOK(vkCreateSemaphore(device, &semi, NULL, &acquire_sem));
   for (int i = 0; i < MAX_IMAGES; i++) VKOK(vkCreateSemaphore(device, &semi, NULL, &render_sem[i]));
   VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VKOK(vkCreateFence(device, &fci, NULL, &fence));

   unsigned frames = 0, resized = 0;
   const unsigned resize_at = frames_wanted / 2;
   while (frames < frames_wanted) {
      // (re)make the swapchain: the first time, and once halfway at another size, the old one handed in as oldSwapchain and destroyed after
      if (cur.sc == VK_NULL_HANDLE || (frames == resize_at && !resized && frames_wanted >= 4)) {
         if (cur.sc != VK_NULL_HANDLE) { W += 64; H += 32; resized = 1; }
         VkSwapchainCreateInfoKHR sci = { .sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR, .surface = surface, .minImageCount = 3,
            .imageFormat = VK_FORMAT_B8G8R8A8_UNORM, .imageColorSpace = VK_COLOR_SPACE_SRGB_NONLINEAR_KHR, .imageExtent = { W, H }, .imageArrayLayers = 1,
            .imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT, .imageSharingMode = VK_SHARING_MODE_EXCLUSIVE,
            .preTransform = VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR, .compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR,
            .presentMode = VK_PRESENT_MODE_FIFO_KHR, .clipped = VK_TRUE, .oldSwapchain = cur.sc };
         old = cur;
         memset(&cur, 0, sizeof(cur));
         VKOK(vkCreateSwapchainKHR(device, &sci, NULL, &cur.sc));
         cur.count = MAX_IMAGES;
         VkResult sr = vkGetSwapchainImagesKHR(device, cur.sc, &cur.count, cur.images);
         CHECK((sr == VK_SUCCESS || sr == VK_INCOMPLETE) && cur.count >= 3, "a swapchain of %u images at %ux%u", cur.count, W, H);
         cur.w = W; cur.h = H;
         if (old.sc != VK_NULL_HANDLE) {
            VKOK(vkDeviceWaitIdle(device));
            vkDestroySwapchainKHR(device, old.sc, NULL);
            old.sc = VK_NULL_HANDLE;
         }
      }

      uint32_t idx = 0;
      VkResult ar = vkAcquireNextImageKHR(device, cur.sc, 5000000000ull, acquire_sem, VK_NULL_HANDLE, &idx);
      if (ar != VK_SUCCESS && ar != VK_SUBOPTIMAL_KHR) { failures++; printf("VK FAIL acquire (%d) at frame %u\n", (int)ar, frames); break; }

      // the frame: a clear to a colour that moves with the frame number (B, G, R, X bytes as XRGB8888)
      VkImageSubresourceRange range = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 };
      VkImageMemoryBarrier to_dst = { .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
         .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED, .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
         .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .image = cur.images[idx], .subresourceRange = range };
      VkImageMemoryBarrier to_present = to_dst;
      to_present.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT; to_present.dstAccessMask = 0;
      to_present.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL; to_present.newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
      VkClearColorValue color = { .float32 = { (frames % 8) / 7.0f, (frames % 5) / 4.0f, (frames % 3) / 2.0f, 1.0f } };
      VkCommandBufferBeginInfo bbi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
      vkResetCommandBuffer(cb, 0);
      vkBeginCommandBuffer(cb, &bbi);
      vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1, &to_dst);
      vkCmdClearColorImage(cb, cur.images[idx], VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, &color, 1, &range);
      vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, NULL, 0, NULL, 1, &to_present);
      vkEndCommandBuffer(cb);
      VkPipelineStageFlags wait_stage = VK_PIPELINE_STAGE_TRANSFER_BIT;
      VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .waitSemaphoreCount = 1, .pWaitSemaphores = &acquire_sem, .pWaitDstStageMask = &wait_stage,
         .commandBufferCount = 1, .pCommandBuffers = &cb, .signalSemaphoreCount = 1, .pSignalSemaphores = &render_sem[idx] };
      vkResetFences(device, 1, &fence);
      if (vkQueueSubmit(queue, 1, &si, fence) != VK_SUCCESS) { failures++; printf("VK FAIL submit at frame %u\n", frames); break; }
      VkPresentInfoKHR pinfo = { .sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR, .waitSemaphoreCount = 1, .pWaitSemaphores = &render_sem[idx],
         .swapchainCount = 1, .pSwapchains = &cur.sc, .pImageIndices = &idx };
      VkResult pr = vkQueuePresentKHR(queue, &pinfo);
      if (pr != VK_SUCCESS && pr != VK_SUBOPTIMAL_KHR) { failures++; printf("VK FAIL present (%d) at frame %u\n", (int)pr, frames); break; }
      if (vkWaitForFences(device, 1, &fence, VK_TRUE, 5000000000ull) != VK_SUCCESS) { failures++; printf("VK FAIL the frame never finished\n"); break; }
      frames++;
   }
   printf("VK window: %u frames presented, %u buffers sent, %u commits, %u releases, %u buffers destroyed so far, %u waits for a release, %u throttled by the compositor (%u timed out)\n", frames, win.buffers_sent, win.commits, win.releases, win.buffers_destroyed, win.waits, win.throttled, win.throttle_timeouts);
   CHECK(frames == frames_wanted, "every frame went through (%u of %u)", frames, frames_wanted);
   CHECK(win.commits == frames, "one commit per present (%u commits, %u frames)", win.commits, frames);
   CHECK(win.buffers_sent == cur.count * (resized ? 2u : 1u), "every image of every swapchain was sent once (%u buffers, %u images now)", win.buffers_sent, cur.count);
   if (device) VKOK(vkDeviceWaitIdle(device));

done:
   if (device) {
      if (cur.sc != VK_NULL_HANDLE) vkDestroySwapchainKHR(device, cur.sc, NULL);
      for (int i = 0; i < MAX_IMAGES; i++) if (render_sem[i]) vkDestroySemaphore(device, render_sem[i], NULL);
      if (acquire_sem) vkDestroySemaphore(device, acquire_sem, NULL);
      if (fence) vkDestroyFence(device, fence, NULL);
      if (cpool) vkDestroyCommandPool(device, cpool, NULL);
   }
   if (surface) vkDestroySurfaceKHR(instance, surface, NULL);
   if (have_window) {
      printf("VK window: %u buffers destroyed in all\n", win.buffers_destroyed);
      gvk_close(&win);
   }
   if (device) vkDestroyDevice(device, NULL);
   if (instance) vkDestroyInstance(instance, NULL);
   if (failures) { printf("VK WINDOW FAILED (%d)\n", failures); return 1; }
   printf("VK WINDOW DONE\n");
   return 0;
}
