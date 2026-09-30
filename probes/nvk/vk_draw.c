/*
 * A Vulkan draw on NVK on constanos (G4e, docs/gpu/g4-nvkmd-plan.md): one triangle covering a 64x64 RGBA8 image in VRAM (dynamic rendering,
 * clear to zero), copied to a host-visible buffer by the copy engine (vkCmdCopyImageToBuffer), every pixel checked by the CPU. Same
 * conventions as vk_probe.c: the ICD's entry points directly, one static musl executable (build.py). On the software device nothing executes
 * and the buffer keeps what the program put there ("not executed"); VK_PROBE_REQUIRE_EXEC=1 makes that a failure.
 */
#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <unwind.h>

#include "draw_spv.h"
#include "tri_spv.h"

#include <fcntl.h>
#include <math.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <time.h>

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
#define VKOK(call) do { VkResult r_ = (call); if (r_ != VK_SUCCESS) { failures++; printf("VK FAIL %s -> %d (line %d)\n", #call, (int)r_, __LINE__); goto done; } else printf("VK ok   %s\n", #call); } while (0)

#define GLOBAL(name) PFN_##name name = (PFN_##name)vk_icdGetInstanceProcAddr(NULL, #name)
#define INST(name) PFN_##name name = (PFN_##name)vk_icdGetInstanceProcAddr(instance, #name)
#define DEV(name) PFN_##name name = (PFN_##name)vkGetDeviceProcAddr(device, #name)

static int find_type(const VkPhysicalDeviceMemoryProperties *mp, uint32_t allowed, VkMemoryPropertyFlags want, VkMemoryPropertyFlags avoid) {
   for (uint32_t i = 0; i < mp->memoryTypeCount; i++)
      if ((allowed & (1u << i)) && (mp->memoryTypes[i].propertyFlags & want) == want && !(mp->memoryTypes[i].propertyFlags & avoid))
         return (int)i;
   return -1;
}

#define W 64
#define H 64

/* /dev/fb0 (docs/reference/graphics.md): stride is in PIXELS (1920 wide -> 2048), as fb0_test.c uses it */
#define FBIO_GET_INFO 0x46420010
#define FBIO_FLUSH 0x46420011
struct fb0_info { uint32_t width, height, stride, bytes_per_pixel; uint64_t offset, map_len; };
struct fb0_rect { uint32_t x, y, w, h; };
struct fb0_flush { uint32_t count, pad; struct fb0_rect rects[16]; };

int main(void) {
   setvbuf(stdout, NULL, _IONBF, 0);
   VkInstance instance = VK_NULL_HANDLE;
   VkDevice device = VK_NULL_HANDLE;
   PFN_vkGetDeviceProcAddr vkGetDeviceProcAddr = NULL;
   VkImage image = VK_NULL_HANDLE;
   VkDeviceMemory imem = VK_NULL_HANDLE, bmem = VK_NULL_HANDLE;
   VkImageView view = VK_NULL_HANDLE;
   VkBuffer buf = VK_NULL_HANDLE;
   VkShaderModule vmod = VK_NULL_HANDLE, fmod = VK_NULL_HANDLE;
   VkPipelineLayout pl = VK_NULL_HANDLE;
   VkPipeline pipe = VK_NULL_HANDLE;
   VkCommandPool cpool = VK_NULL_HANDLE;
   VkFence fence = VK_NULL_HANDLE;
   VkImage image2 = VK_NULL_HANDLE;
   VkDeviceMemory imem2 = VK_NULL_HANDLE, bmem2 = VK_NULL_HANDLE;
   VkImageView view2 = VK_NULL_HANDLE;
   VkBuffer buf2 = VK_NULL_HANDLE;
   VkShaderModule vmod2 = VK_NULL_HANDLE, fmod2 = VK_NULL_HANDLE;
   VkPipelineLayout pl2 = VK_NULL_HANDLE;
   VkPipeline pipe2 = VK_NULL_HANDLE;

   GLOBAL(vkCreateInstance);
   if (!vkCreateInstance) return 1;
   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .pApplicationName = "vk_draw", .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app };
   VKOK(vkCreateInstance(&ici, NULL, &instance));
   INST(vkEnumeratePhysicalDevices);
   INST(vkGetPhysicalDeviceQueueFamilyProperties);
   INST(vkGetPhysicalDeviceMemoryProperties);
   INST(vkCreateDevice);
   vkGetDeviceProcAddr = (PFN_vkGetDeviceProcAddr)vk_icdGetInstanceProcAddr(instance, "vkGetDeviceProcAddr");

   uint32_t n = 1;
   VkPhysicalDevice pdev = VK_NULL_HANDLE;
   VkResult r = vkEnumeratePhysicalDevices(instance, &n, &pdev);
   CHECK((r == VK_SUCCESS || r == VK_INCOMPLETE) && pdev != VK_NULL_HANDLE, "a physical device (%d)", (int)r);
   if (!pdev) goto done;

   uint32_t nq = 0;
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nq, NULL);
   VkQueueFamilyProperties qf[16];
   if (nq > 16) nq = 16;
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nq, qf);
   int family = -1;
   for (uint32_t i = 0; i < nq; i++)
      if (family < 0 && (qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT)) family = (int)i;
   printf("VK using queue family %d\n", family);
   CHECK(family >= 0, "a graphics queue family");
   if (family < 0) goto done;

   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(pdev, &mp);

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = family, .queueCount = 1, .pQueuePriorities = &prio };
   VkPhysicalDeviceVulkan13Features f13 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES, .dynamicRendering = VK_TRUE };
   VkPhysicalDeviceVulkan12Features f12 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES, .pNext = &f13, .timelineSemaphore = VK_TRUE };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &f12, .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci };
   VKOK(vkCreateDevice(pdev, &dci, NULL, &device));

   DEV(vkDestroyDevice); DEV(vkGetDeviceQueue); DEV(vkCreateBuffer); DEV(vkGetBufferMemoryRequirements); DEV(vkAllocateMemory);
   DEV(vkBindBufferMemory); DEV(vkMapMemory); DEV(vkCreateImage); DEV(vkGetImageMemoryRequirements); DEV(vkBindImageMemory);
   DEV(vkCreateImageView); DEV(vkCreateShaderModule); DEV(vkCreatePipelineLayout); DEV(vkCreateGraphicsPipelines);
   DEV(vkCreateCommandPool); DEV(vkAllocateCommandBuffers); DEV(vkBeginCommandBuffer); DEV(vkEndCommandBuffer);
   DEV(vkCmdBeginRendering); DEV(vkCmdEndRendering); DEV(vkCmdBindPipeline); DEV(vkCmdSetViewport); DEV(vkCmdSetScissor); DEV(vkCmdDraw);
   DEV(vkCmdPipelineBarrier); DEV(vkCmdCopyImageToBuffer); DEV(vkCreateFence); DEV(vkQueueSubmit); DEV(vkWaitForFences); DEV(vkDeviceWaitIdle);

   VkQueue queue;
   vkGetDeviceQueue(device, family, 0, &queue);

   // ---- the image (device-local: VRAM) and the buffer it is copied to (host-visible)
   VkImageCreateInfo imci = { .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .imageType = VK_IMAGE_TYPE_2D, .format = VK_FORMAT_B8G8R8A8_UNORM,
      .extent = { W, H, 1 }, .mipLevels = 1, .arrayLayers = 1, .samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_OPTIMAL,
      .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT, .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED };
   VKOK(vkCreateImage(device, &imci, NULL, &image));
   VkMemoryRequirements req;
   vkGetImageMemoryRequirements(device, image, &req);
   int it = find_type(&mp, req.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT);
   CHECK(it >= 0, "a device-local memory type for the image");
   if (it < 0) goto done;
   printf("VK image needs %llu bytes, align %llu\n", (unsigned long long)req.size, (unsigned long long)req.alignment);
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = req.size, .memoryTypeIndex = it };
   VKOK(vkAllocateMemory(device, &mai, NULL, &imem));
   VKOK(vkBindImageMemory(device, image, imem, 0));
   VkImageViewCreateInfo ivci = { .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .image = image, .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = VK_FORMAT_B8G8R8A8_UNORM,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
   VKOK(vkCreateImageView(device, &ivci, NULL, &view));

   VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = W * H * 4, .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT, .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
   VKOK(vkCreateBuffer(device, &bci, NULL, &buf));
   vkGetBufferMemoryRequirements(device, buf, &req);
   int bt = find_type(&mp, req.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT, 0);
   CHECK(bt >= 0, "a host-visible coherent type for the buffer");
   if (bt < 0) goto done;
   mai.allocationSize = req.size;
   mai.memoryTypeIndex = bt;
   VKOK(vkAllocateMemory(device, &mai, NULL, &bmem));
   VKOK(vkBindBufferMemory(device, buf, bmem, 0));
   uint32_t *px = NULL;
   VKOK(vkMapMemory(device, bmem, 0, VK_WHOLE_SIZE, 0, (void **)&px));
   for (uint32_t i = 0; i < W * H; i++) px[i] = 0xdeadbeefu;

   // ---- the pipeline: a vertex shader that makes one big triangle, a fragment shader with a constant colour
   VkShaderModuleCreateInfo vs = { .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = draw_vert_spv_len, .pCode = (const uint32_t *)draw_vert_spv };
   VkShaderModuleCreateInfo fs = { .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = draw_frag_spv_len, .pCode = (const uint32_t *)draw_frag_spv };
   VKOK(vkCreateShaderModule(device, &vs, NULL, &vmod));
   VKOK(vkCreateShaderModule(device, &fs, NULL, &fmod));
   VkPipelineLayoutCreateInfo plci = { .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO };
   VKOK(vkCreatePipelineLayout(device, &plci, NULL, &pl));
   VkPipelineShaderStageCreateInfo stages[2] = {
      { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_VERTEX_BIT, .module = vmod, .pName = "main" },
      { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_FRAGMENT_BIT, .module = fmod, .pName = "main" } };
   VkPipelineVertexInputStateCreateInfo vi = { .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO };
   VkPipelineInputAssemblyStateCreateInfo ia = { .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO, .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST };
   VkPipelineViewportStateCreateInfo vp = { .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO, .viewportCount = 1, .scissorCount = 1 };
   VkPipelineRasterizationStateCreateInfo rs = { .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO, .polygonMode = VK_POLYGON_MODE_FILL,
      .cullMode = VK_CULL_MODE_NONE, .frontFace = VK_FRONT_FACE_COUNTER_CLOCKWISE, .lineWidth = 1.0f };
   VkPipelineMultisampleStateCreateInfo ms = { .sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO, .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT };
   VkPipelineColorBlendAttachmentState cba = { .colorWriteMask = 0xf };
   VkPipelineColorBlendStateCreateInfo cb = { .sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO, .attachmentCount = 1, .pAttachments = &cba };
   VkDynamicState dyn[2] = { VK_DYNAMIC_STATE_VIEWPORT, VK_DYNAMIC_STATE_SCISSOR };
   VkPipelineDynamicStateCreateInfo ds = { .sType = VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO, .dynamicStateCount = 2, .pDynamicStates = dyn };
   VkFormat cf = VK_FORMAT_B8G8R8A8_UNORM;
   VkPipelineRenderingCreateInfo pri = { .sType = VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO, .colorAttachmentCount = 1, .pColorAttachmentFormats = &cf };
   VkGraphicsPipelineCreateInfo gpci = { .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, .pNext = &pri, .stageCount = 2, .pStages = stages,
      .pVertexInputState = &vi, .pInputAssemblyState = &ia, .pViewportState = &vp, .pRasterizationState = &rs, .pMultisampleState = &ms,
      .pColorBlendState = &cb, .pDynamicState = &ds, .layout = pl };
   VKOK(vkCreateGraphicsPipelines(device, VK_NULL_HANDLE, 1, &gpci, NULL, &pipe));

   // ---- commands: clear + draw, then the image to the buffer
   VkCommandPoolCreateInfo cpi = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = family };
   VKOK(vkCreateCommandPool(device, &cpi, NULL, &cpool));
   VkCommandBufferAllocateInfo cbai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = cpool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
   VkCommandBuffer cmd;
   VKOK(vkAllocateCommandBuffers(device, &cbai, &cmd));
   VkCommandBufferBeginInfo bbi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
   VKOK(vkBeginCommandBuffer(cmd, &bbi));
   VkImageMemoryBarrier to_color = { .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
      .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED, .newLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .image = image, .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, 0, 0, NULL, 0, NULL, 1, &to_color);
   VkRenderingAttachmentInfo ca = { .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO, .imageView = view, .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
      .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR, .storeOp = VK_ATTACHMENT_STORE_OP_STORE, .clearValue = { .color = { .float32 = { 0, 0, 0, 0 } } } };
   VkRenderingInfo ri = { .sType = VK_STRUCTURE_TYPE_RENDERING_INFO, .renderArea = { { 0, 0 }, { W, H } }, .layerCount = 1, .colorAttachmentCount = 1, .pColorAttachments = &ca };
   vkCmdBeginRendering(cmd, &ri);
   VkViewport viewport = { 0, 0, W, H, 0.0f, 1.0f };
   VkRect2D scissor = { { 0, 0 }, { W, H } };
   vkCmdSetViewport(cmd, 0, 1, &viewport);
   vkCmdSetScissor(cmd, 0, 1, &scissor);
   vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pipe);
   vkCmdDraw(cmd, 3, 1, 0, 0);
   vkCmdEndRendering(cmd);
   VkImageMemoryBarrier to_src = { .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT, .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
      .oldLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, .newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .image = image, .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1, &to_src);
   VkBufferImageCopy region = { .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 }, .imageExtent = { W, H, 1 } };
   vkCmdCopyImageToBuffer(cmd, image, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, buf, 1, &region);
   VkMemoryBarrier to_host = { .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER, .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT, .dstAccessMask = VK_ACCESS_HOST_READ_BIT };
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0, 1, &to_host, 0, NULL, 0, NULL);
   VKOK(vkEndCommandBuffer(cmd));

   VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VKOK(vkCreateFence(device, &fci, NULL, &fence));
   VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cmd };
   VKOK(vkQueueSubmit(queue, 1, &si, fence));
   r = vkWaitForFences(device, 1, &fence, VK_TRUE, 10000000000ull);
   CHECK(r == VK_SUCCESS, "the fence of the draw signals (%d)", (int)r);

   // ---- every pixel: (255, 128, 64, 255), give or take one in the rounding of 0.5 and 0.25
   unsigned good = 0, untouched = 0, first_bad = ~0u;
   for (uint32_t i = 0; i < W * H; i++) {
      uint32_t p = px[i];
      // B8G8R8A8: byte 0 is blue, byte 2 red (a 0x00RRGGBB pixel, what /dev/fb0 takes)
      int bb = p & 0xff, gg = (p >> 8) & 0xff, rr = (p >> 16) & 0xff, aa = p >> 24;
      if (rr == 255 && (gg == 127 || gg == 128) && (bb == 63 || bb == 64) && aa == 255) good++;
      else { if (first_bad == ~0u) first_bad = i; if (p == 0xdeadbeefu) untouched++; }
   }
   int executed = good == W * H;
   printf("VK draw result: %s (%u of %u pixels right, %u untouched, first wrong pixel %d = %#x)\n", executed ? "EXECUTED" : untouched == W * H - good && good == 0 ? "not executed (software device)" : "WRONG DATA",
          good, W * H, untouched, first_bad == ~0u ? -1 : (int)first_bad, first_bad == ~0u ? 0 : px[first_bad]);
   CHECK(executed || untouched == W * H, "the buffer holds either the drawn image or what we put there");
   if (getenv("VK_PROBE_REQUIRE_EXEC")) CHECK(executed, "the draw really ran on the GPU");
   VKOK(vkDeviceWaitIdle(device));

   // ---- G4e presentation: VK_DRAW_PRESENT=<seconds> keeps drawing a spinning triangle into a 640x400 image, copies each frame to a host buffer
   // and from there into /dev/fb0, so the GPU's pictures reach the screen (by the CPU for now; zero-copy scanout is for later)
   if (getenv("VK_DRAW_PRESENT") && executed) {
      const uint32_t PW = 640, PH = 400;
      double seconds = atof(getenv("VK_DRAW_PRESENT"));
      int fb = open("/dev/fb0", O_RDWR);
      struct fb0_info fi;
      uint8_t *fbmap = MAP_FAILED;
      if (fb >= 0 && ioctl(fb, FBIO_GET_INFO, &fi) == 0) fbmap = mmap(NULL, fi.map_len, PROT_READ | PROT_WRITE, MAP_SHARED, fb, 0);
      CHECK(fbmap != MAP_FAILED, "/dev/fb0 mapped");
      if (fbmap == MAP_FAILED) goto done;
      printf("VK present: screen %ux%u stride %u, image %ux%u\n", fi.width, fi.height, fi.stride, PW, PH);
      imci.extent = (VkExtent3D){ PW, PH, 1 };
      VKOK(vkCreateImage(device, &imci, NULL, &image2));
      vkGetImageMemoryRequirements(device, image2, &req);
      mai.allocationSize = req.size;
      mai.memoryTypeIndex = it;
      VKOK(vkAllocateMemory(device, &mai, NULL, &imem2));
      VKOK(vkBindImageMemory(device, image2, imem2, 0));
      ivci.image = image2;
      VKOK(vkCreateImageView(device, &ivci, NULL, &view2));
      bci.size = PW * PH * 4;
      VKOK(vkCreateBuffer(device, &bci, NULL, &buf2));
      vkGetBufferMemoryRequirements(device, buf2, &req);
      mai.allocationSize = req.size;
      mai.memoryTypeIndex = bt;
      VKOK(vkAllocateMemory(device, &mai, NULL, &bmem2));
      VKOK(vkBindBufferMemory(device, buf2, bmem2, 0));
      uint32_t *px2 = NULL;
      VKOK(vkMapMemory(device, bmem2, 0, VK_WHOLE_SIZE, 0, (void **)&px2));

      VkShaderModuleCreateInfo tvs = { .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = tri_vert_spv_len, .pCode = (const uint32_t *)tri_vert_spv };
      VkShaderModuleCreateInfo tfs = { .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = tri_frag_spv_len, .pCode = (const uint32_t *)tri_frag_spv };
      VKOK(vkCreateShaderModule(device, &tvs, NULL, &vmod2));
      VKOK(vkCreateShaderModule(device, &tfs, NULL, &fmod2));
      VkPushConstantRange pcr = { VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT, 0, 32 };
      VkPipelineLayoutCreateInfo plci2 = { .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, .pushConstantRangeCount = 1, .pPushConstantRanges = &pcr };
      VKOK(vkCreatePipelineLayout(device, &plci2, NULL, &pl2));
      stages[0].module = vmod2;
      stages[1].module = fmod2;
      gpci.layout = pl2;
      VKOK(vkCreateGraphicsPipelines(device, VK_NULL_HANDLE, 1, &gpci, NULL, &pipe2));
      VkCommandBuffer cmd2;
      VKOK(vkAllocateCommandBuffers(device, &cbai, &cmd2));
      VkFence fence2;
      VKOK(vkCreateFence(device, &fci, NULL, &fence2));
      PFN_vkResetFences vkResetFences = (PFN_vkResetFences)vkGetDeviceProcAddr(device, "vkResetFences");
      PFN_vkResetCommandBuffer vkResetCommandBuffer = (PFN_vkResetCommandBuffer)vkGetDeviceProcAddr(device, "vkResetCommandBuffer");
      PFN_vkCmdPushConstants vkCmdPushConstants = (PFN_vkCmdPushConstants)vkGetDeviceProcAddr(device, "vkCmdPushConstants");
      VkImageMemoryBarrier b1 = to_color, b2 = to_src;
      b1.image = b2.image = image2;
      b1.oldLayout = VK_IMAGE_LAYOUT_UNDEFINED;
      VkRenderingAttachmentInfo ca2 = ca;
      ca2.imageView = view2;
      ca2.clearValue.color.float32[0] = 0.02f; ca2.clearValue.color.float32[1] = 0.05f; ca2.clearValue.color.float32[2] = 0.12f; ca2.clearValue.color.float32[3] = 1.0f;
      VkRenderingInfo ri2 = ri;
      ri2.renderArea.extent = (VkExtent2D){ PW, PH };
      ri2.pColorAttachments = &ca2;
      VkViewport vp2 = { 0, 0, PW, PH, 0.0f, 1.0f };
      VkRect2D sc2 = { { 0, 0 }, { PW, PH } };
      VkBufferImageCopy reg2 = region;
      reg2.imageExtent = (VkExtent3D){ PW, PH, 1 };
      const uint32_t x0 = (fi.width - PW) / 2, y0 = (fi.height - PH) / 2;
      struct timespec t0, t1;
      clock_gettime(CLOCK_MONOTONIC, &t0);
      unsigned frames = 0, flush_busy = 0;
      int frame_ok = 1;
      for (;;) {
         clock_gettime(CLOCK_MONOTONIC, &t1);
         double el = (t1.tv_sec - t0.tv_sec) + (t1.tv_nsec - t0.tv_nsec) / 1e9;
         if (el >= seconds && frames > 0) break;
         float angle = (float)el * 1.5f;
         float pc[8] = { angle, (float)PW / (float)PH, 0, 0, 0.5f + 0.5f * sinf(angle), 0.5f + 0.5f * sinf(angle + 2.1f), 0.5f + 0.5f * sinf(angle + 4.2f), 1.0f };
         for (uint32_t i = 0; i < PW * PH; i++) px2[i] = 0xdeadbeefu;
         vkResetCommandBuffer(cmd2, 0);
         vkBeginCommandBuffer(cmd2, &bbi);
         vkCmdPipelineBarrier(cmd2, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, 0, 0, NULL, 0, NULL, 1, &b1);
         vkCmdBeginRendering(cmd2, &ri2);
         vkCmdSetViewport(cmd2, 0, 1, &vp2);
         vkCmdSetScissor(cmd2, 0, 1, &sc2);
         vkCmdBindPipeline(cmd2, VK_PIPELINE_BIND_POINT_GRAPHICS, pipe2);
         vkCmdPushConstants(cmd2, pl2, VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT, 0, 32, pc);
         vkCmdDraw(cmd2, 3, 1, 0, 0);
         vkCmdEndRendering(cmd2);
         vkCmdPipelineBarrier(cmd2, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1, &b2);
         vkCmdCopyImageToBuffer(cmd2, image2, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, buf2, 1, &reg2);
         vkCmdPipelineBarrier(cmd2, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0, 1, &to_host, 0, NULL, 0, NULL);
         vkEndCommandBuffer(cmd2);
         vkResetFences(device, 1, &fence2);
         VkSubmitInfo si3 = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cmd2 };
         if (vkQueueSubmit(queue, 1, &si3, fence2) != VK_SUCCESS || vkWaitForFences(device, 1, &fence2, VK_TRUE, 10000000000ull) != VK_SUCCESS) { frame_ok = 0; break; }
         if (frames == 0) {
            // the clear colour in the corner, and something that is neither it nor the garbage at the centre of the triangle
            uint32_t corner = px2[0], centre = px2[(PH / 2) * PW + PW / 2];
            printf("VK present: frame 0 corner %#x centre %#x\n", corner, centre);
            CHECK(corner != 0xdeadbeefu && centre != 0xdeadbeefu && corner != centre, "frame 0: the GPU wrote the image and the triangle is not the background");
         }
         for (uint32_t y = 0; y < PH; y++) memcpy(fbmap + fi.offset + ((size_t)(y0 + y) * fi.stride + x0) * 4, px2 + (size_t)y * PW, PW * 4);
         if (frames == 0) {
            // what reached the framebuffer's memory is the image, row by row (this is the check a wrong stride fails)
            int rows_ok = 1;
            for (uint32_t y = 0; y < PH; y++) rows_ok &= memcmp(fbmap + fi.offset + ((size_t)(y0 + y) * fi.stride + x0) * 4, px2 + (size_t)y * PW, PW * 4) == 0;
            CHECK(rows_ok, "frame 0 is in the framebuffer, every row where it belongs");
            CHECK(*(uint32_t *)(fbmap + fi.offset + ((size_t)(y0 + 1) * fi.stride + x0) * 4) == px2[PW], "the second row starts a stride below the first");
         }
         struct fb0_flush fl = { .count = 1, .rects = { { x0, y0, PW, PH } } };
         if (ioctl(fb, FBIO_FLUSH, &fl) != 0) flush_busy++;
         frames++;
         usleep(16000);   // about one frame per vblank: a flip is pending until the next one, and flooding FBIO_FLUSH starves the vblank handler
      }
      CHECK(frame_ok, "every frame rendered and fenced");
      printf("VK present: %u frames in %.1f s (%.1f fps), %u flushes refused (a flip pending)\n", frames, seconds, frames / (seconds > 0 ? seconds : 1), flush_busy);
      VKOK(vkDeviceWaitIdle(device));
   }

done:
   if (device) {
      PFN_vkDeviceWaitIdle wi = (PFN_vkDeviceWaitIdle)vkGetDeviceProcAddr(device, "vkDeviceWaitIdle");
      if (wi) wi(device);
#define GONE(handle, fn) do { PFN_##fn f_ = (PFN_##fn)vkGetDeviceProcAddr(device, #fn); if (handle && f_) f_(device, handle, NULL); } while (0)
      GONE(pipe2, vkDestroyPipeline);
      GONE(pl2, vkDestroyPipelineLayout);
      GONE(fmod2, vkDestroyShaderModule);
      GONE(vmod2, vkDestroyShaderModule);
      GONE(buf2, vkDestroyBuffer);
      GONE(view2, vkDestroyImageView);
      GONE(image2, vkDestroyImage);
      GONE(bmem2, vkFreeMemory);
      GONE(imem2, vkFreeMemory);
      GONE(fence, vkDestroyFence);
      GONE(cpool, vkDestroyCommandPool);
      GONE(pipe, vkDestroyPipeline);
      GONE(pl, vkDestroyPipelineLayout);
      GONE(fmod, vkDestroyShaderModule);
      GONE(vmod, vkDestroyShaderModule);
      GONE(buf, vkDestroyBuffer);
      GONE(view, vkDestroyImageView);
      GONE(image, vkDestroyImage);
      GONE(bmem, vkFreeMemory);
      GONE(imem, vkFreeMemory);
      PFN_vkDestroyDevice dd = (PFN_vkDestroyDevice)vkGetDeviceProcAddr(device, "vkDestroyDevice");
      if (dd) dd(device, NULL);
   }
   if (instance) {
      PFN_vkDestroyInstance di = (PFN_vkDestroyInstance)vk_icdGetInstanceProcAddr(instance, "vkDestroyInstance");
      if (di) di(instance, NULL);
   }
   if (failures) { printf("VK DRAW FAILED (%d)\n", failures); return 1; }
   printf("VK DRAW DONE\n");
   return 0;
}
