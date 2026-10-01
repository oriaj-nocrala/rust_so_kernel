/*
 * comp_vk.c: the GPU compositor's renderer as a C library for the program in Rust (vk-comp/): Vulkan bring-up on NVK, the screen through the WSI's
 * direct path (a headless surface is the display), the swapchain, and comp_render.h's pipeline. The entry points are `cr_*` (comp_api.h). The
 * window manager, the sockets, the input and the titles are the Rust program's.
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

#include "comp_render.h"

static _Unwind_Reason_Code trace_cb(struct _Unwind_Context *c, void *arg) {
   (void)arg;
   printf("COMP BT %#lx\n", (unsigned long)_Unwind_GetIP(c));
   return _URC_NO_REASON;
}

void __assert_fail(const char *expr, const char *file, int line, const char *func) {
   printf("COMP ASSERT %s (%s: %s: %d)\n", expr, file, func, line);
   _Unwind_Backtrace(trace_cb, NULL);
   _exit(134);
}

void abort(void) {
   _Unwind_Backtrace(trace_cb, NULL);
   _exit(134);
}

extern PFN_vkVoidFunction vk_icdGetInstanceProcAddr(VkInstance instance, const char *name);
/* NVK's constanos extension (nvkmd_constanos.c): sleeps (poll on /dev/vblank) until the last present has taken effect: 0, or -ETIMEDOUT / another negative errno. */
extern int nvk_constanos_wait_flip(VkDevice device, int timeout_ms);

#define MAX_SC_IMAGES 8

static struct {
   VkInstance instance;
   VkDevice device;
   VkSurfaceKHR surface;
   VkSwapchainKHR swapchain;
   VkImage images[MAX_SC_IMAGES];
   VkImageView views[MAX_SC_IMAGES];
   VkSemaphore render_sem[MAX_SC_IMAGES];
   VkSemaphore acquire_sem;
   uint32_t count, w, h;
   VkQueue queue;
   struct comp comp;
   int comp_ready;
   uint64_t frames;
   uint32_t acquire_us, render_us, present_us;   /* the last cr_frame's phases */
   PFN_vkGetDeviceProcAddr gdpa;
   PFN_vkDestroySwapchainKHR vkDestroySwapchainKHR;
   PFN_vkAcquireNextImageKHR vkAcquireNextImageKHR;
   PFN_vkQueuePresentKHR vkQueuePresentKHR;
   PFN_vkDestroySemaphore vkDestroySemaphore;
   PFN_vkDestroyImageView vkDestroyImageView;
   PFN_vkDestroyDevice vkDestroyDevice;
   PFN_vkDestroySurfaceKHR vkDestroySurfaceKHR;
   PFN_vkDestroyInstance vkDestroyInstance;
} R;

#define FAILSTEP(...) do { printf("COMP FAIL "); printf(__VA_ARGS__); printf("\n"); return -1; } while (0)

int cr_init(int headless, uint32_t *width, uint32_t *height) {
   memset(&R, 0, sizeof(R));
   PFN_vkCreateInstance vkCreateInstance = (PFN_vkCreateInstance)vk_icdGetInstanceProcAddr(NULL, "vkCreateInstance");
   if (!vkCreateInstance) FAILSTEP("no vkCreateInstance");
   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .pApplicationName = "vk_comp", .apiVersion = VK_API_VERSION_1_3 };
   static const char *const instance_exts[] = { "VK_KHR_surface", "VK_EXT_headless_surface" };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app, .enabledExtensionCount = 2, .ppEnabledExtensionNames = instance_exts };
   if (vkCreateInstance(&ici, NULL, &R.instance) != VK_SUCCESS) FAILSTEP("vkCreateInstance");
   VkInstance instance = R.instance;
#define INST(name) PFN_##name name = (PFN_##name)vk_icdGetInstanceProcAddr(instance, #name)
   INST(vkEnumeratePhysicalDevices);
   INST(vkGetPhysicalDeviceQueueFamilyProperties);
   INST(vkGetPhysicalDeviceMemoryProperties);
   INST(vkCreateDevice);
   INST(vkCreateHeadlessSurfaceEXT);
   INST(vkGetPhysicalDeviceSurfaceSupportKHR);
   INST(vkGetPhysicalDeviceSurfaceCapabilitiesKHR);
   R.vkDestroySurfaceKHR = (PFN_vkDestroySurfaceKHR)vk_icdGetInstanceProcAddr(instance, "vkDestroySurfaceKHR");
   R.vkDestroyInstance = (PFN_vkDestroyInstance)vk_icdGetInstanceProcAddr(instance, "vkDestroyInstance");
   R.gdpa = (PFN_vkGetDeviceProcAddr)vk_icdGetInstanceProcAddr(instance, "vkGetDeviceProcAddr");

   uint32_t npd = 1;
   VkPhysicalDevice pdev = VK_NULL_HANDLE;
   VkResult er = vkEnumeratePhysicalDevices(instance, &npd, &pdev);
   if ((er != VK_SUCCESS && er != VK_INCOMPLETE) || !pdev) FAILSTEP("no physical device: is gpu=uapi on, and /dev/nvgpu free?");
   uint32_t nq = 16;
   VkQueueFamilyProperties qf[16];
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nq, qf);
   int family = -1;
   for (uint32_t i = 0; i < nq && family < 0; i++) if (qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) family = (int)i;
   if (family < 0) FAILSTEP("no graphics queue family");
   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(pdev, &mp);
   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = (uint32_t)family, .queueCount = 1, .pQueuePriorities = &prio };
   static const char *const device_exts[] = { "VK_KHR_swapchain", "VK_KHR_external_memory_fd" };
   VkPhysicalDeviceVulkan13Features f13 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES, .dynamicRendering = VK_TRUE };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &f13, .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
                              .enabledExtensionCount = 2, .ppEnabledExtensionNames = device_exts };
   if (vkCreateDevice(pdev, &dci, NULL, &R.device) != VK_SUCCESS) FAILSTEP("vkCreateDevice");
   VkDevice device = R.device;
#define DEV(name) PFN_##name name = (PFN_##name)R.gdpa(device, #name)
   DEV(vkCreateSwapchainKHR); DEV(vkGetSwapchainImagesKHR); DEV(vkCreateSemaphore); DEV(vkCreateImageView); DEV(vkGetDeviceQueue);
   R.vkDestroySwapchainKHR = (PFN_vkDestroySwapchainKHR)R.gdpa(device, "vkDestroySwapchainKHR");
   R.vkAcquireNextImageKHR = (PFN_vkAcquireNextImageKHR)R.gdpa(device, "vkAcquireNextImageKHR");
   R.vkQueuePresentKHR = (PFN_vkQueuePresentKHR)R.gdpa(device, "vkQueuePresentKHR");
   R.vkDestroySemaphore = (PFN_vkDestroySemaphore)R.gdpa(device, "vkDestroySemaphore");
   R.vkDestroyImageView = (PFN_vkDestroyImageView)R.gdpa(device, "vkDestroyImageView");
   R.vkDestroyDevice = (PFN_vkDestroyDevice)R.gdpa(device, "vkDestroyDevice");
   vkGetDeviceQueue(device, (uint32_t)family, 0, &R.queue);

   /* the screen: a headless surface is the display when the driver has one */
   VkHeadlessSurfaceCreateInfoEXT hsci = { .sType = VK_STRUCTURE_TYPE_HEADLESS_SURFACE_CREATE_INFO_EXT };
   if (vkCreateHeadlessSurfaceEXT(instance, &hsci, NULL, &R.surface) != VK_SUCCESS) FAILSTEP("vkCreateHeadlessSurfaceEXT");
   VkBool32 supported = VK_FALSE;
   vkGetPhysicalDeviceSurfaceSupportKHR(pdev, (uint32_t)family, R.surface, &supported);
   VkSurfaceCapabilitiesKHR caps;
   if (vkGetPhysicalDeviceSurfaceCapabilitiesKHR(pdev, R.surface, &caps) != VK_SUCCESS) FAILSTEP("surface capabilities");
   const int has_display = caps.currentExtent.width != 0xffffffffu && caps.currentExtent.width != 0;
   if (!has_display && !headless) FAILSTEP("no display behind the surface: is gpu=uapi on, and nothing else holding the screen? (COMP_HEADLESS=1 to run without one)");
   R.w = has_display ? caps.currentExtent.width : 640;
   R.h = has_display ? caps.currentExtent.height : 360;
   printf("COMP screen %ux%u (%s)\n", R.w, R.h, has_display ? "the display" : "headless");
   VkSwapchainCreateInfoKHR sci = { .sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR, .surface = R.surface, .minImageCount = caps.minImageCount < 3 ? 3 : caps.minImageCount,
      .imageFormat = VK_FORMAT_B8G8R8A8_UNORM, .imageColorSpace = VK_COLOR_SPACE_SRGB_NONLINEAR_KHR, .imageExtent = { R.w, R.h }, .imageArrayLayers = 1,
      .imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT, .imageSharingMode = VK_SHARING_MODE_EXCLUSIVE, .preTransform = VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR,
      .compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR, .presentMode = VK_PRESENT_MODE_FIFO_KHR, .clipped = VK_TRUE };
   if (vkCreateSwapchainKHR(device, &sci, NULL, &R.swapchain) != VK_SUCCESS) FAILSTEP("vkCreateSwapchainKHR");
   R.count = MAX_SC_IMAGES;
   VkResult sr = vkGetSwapchainImagesKHR(device, R.swapchain, &R.count, R.images);
   if ((sr != VK_SUCCESS && sr != VK_INCOMPLETE) || R.count == 0) FAILSTEP("swapchain images (%d)", (int)sr);
   VkSemaphoreCreateInfo semi = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO };
   if (vkCreateSemaphore(device, &semi, NULL, &R.acquire_sem) != VK_SUCCESS) FAILSTEP("semaphore");
   VkImageViewCreateInfo ivci = { .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = VK_FORMAT_B8G8R8A8_UNORM,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
   for (uint32_t i = 0; i < R.count; i++) {
      ivci.image = R.images[i];
      if (vkCreateImageView(device, &ivci, NULL, &R.views[i]) != VK_SUCCESS || vkCreateSemaphore(device, &semi, NULL, &R.render_sem[i]) != VK_SUCCESS) FAILSTEP("image view or semaphore");
   }
   int rc = comp_init(&R.comp, device, R.gdpa, &mp, (uint32_t)family, VK_FORMAT_B8G8R8A8_UNORM);
   if (rc) FAILSTEP("the renderer did not start (step %d)", rc);
   R.comp_ready = 1;
   *width = R.w;
   *height = R.h;
   return 0;
}

int cr_import(uint64_t handle, int fd, uint64_t size, uint32_t stride_bytes) {
   int r = comp_import(&R.comp, handle, fd, size, stride_bytes);
   if (r) printf("COMP the import of buffer %llu failed (%d)\n", (unsigned long long)handle, r);
   return r;
}

void cr_drop(uint64_t handle) { comp_drop(&R.comp, handle); }

uint64_t cr_wait(void) { return comp_wait_frame(&R.comp); }

static uint64_t now_us(void) {
   struct timespec t;
   clock_gettime(CLOCK_MONOTONIC, &t);
   return (uint64_t)t.tv_sec * 1000000u + (uint64_t)t.tv_nsec / 1000u;
}

int cr_frame(const struct cr_op *ops, size_t n, uint64_t epoch) {
   uint32_t idx = 0;
   const uint64_t t0 = now_us();
   VkResult ar = R.vkAcquireNextImageKHR(R.device, R.swapchain, 1000000000ull, R.acquire_sem, VK_NULL_HANDLE, &idx);
   if (ar != VK_SUCCESS && ar != VK_SUBOPTIMAL_KHR) { printf("COMP FAIL acquire (%d) at frame %llu\n", (int)ar, (unsigned long long)R.frames); return -1; }
   const uint64_t t1 = now_us();
   if (comp_frame(&R.comp, ops, n, epoch, R.images[idx], R.views[idx], R.w, R.h, R.acquire_sem, R.render_sem[idx], VK_IMAGE_LAYOUT_PRESENT_SRC_KHR) != 0) {
      printf("COMP FAIL frame %llu\n", (unsigned long long)R.frames);
      return -2;
   }
   const uint64_t t2 = now_us();
   VkPresentInfoKHR pinfo = { .sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR, .waitSemaphoreCount = 1, .pWaitSemaphores = &R.render_sem[idx],
      .swapchainCount = 1, .pSwapchains = &R.swapchain, .pImageIndices = &idx };
   VkResult pr = R.vkQueuePresentKHR(R.queue, &pinfo);
   if (pr != VK_SUCCESS && pr != VK_SUBOPTIMAL_KHR) { printf("COMP FAIL present (%d) at frame %llu\n", (int)pr, (unsigned long long)R.frames); return -3; }
   const uint64_t t3 = now_us();
   R.acquire_us = (uint32_t)(t1 - t0);
   R.render_us = (uint32_t)(t2 - t1);
   R.present_us = (uint32_t)(t3 - t2);
   R.frames++;
   return 0;
}

int cr_wait_flip(void) {
   /* the frame just presented is on screen once its flip has landed (the next vblank): headless or without a display this returns at once */
   return nvk_constanos_wait_flip(R.device, 100);
}

void cr_get_stats(struct cr_stats *out) {
   out->frames = R.comp.frames;
   out->draws = R.comp.draws;
   out->draws_max = R.comp.draws_max;
   out->imports = R.comp.imports;
   out->drops = R.comp.drops;
   out->uploads = R.comp.uploads;
   out->acquire_us = R.acquire_us;
   out->render_us = R.render_us;
   out->present_us = R.present_us;
}

void cr_shutdown(void) {
   if (R.comp_ready) comp_destroy(&R.comp);
   if (R.device) {
      for (uint32_t i = 0; i < R.count; i++) {
         if (R.views[i]) R.vkDestroyImageView(R.device, R.views[i], NULL);
         if (R.render_sem[i]) R.vkDestroySemaphore(R.device, R.render_sem[i], NULL);
      }
      if (R.acquire_sem) R.vkDestroySemaphore(R.device, R.acquire_sem, NULL);
      if (R.swapchain) R.vkDestroySwapchainKHR(R.device, R.swapchain, NULL);
   }
   if (R.surface) R.vkDestroySurfaceKHR(R.instance, R.surface, NULL);
   if (R.device) R.vkDestroyDevice(R.device, NULL);
   if (R.instance) R.vkDestroyInstance(R.instance, NULL);
   memset(&R, 0, sizeof(R));
}
