/*
 * comp_render.h: the GPU compositor's renderer (G5 layer 4, slice 3; docs/gpu/g5-graphics-stack-plan.md). Header-only, shared by
 * vk_comp.c (constanos: NVK linked in, the screen) and the host harness (-DCOMP_HOST: the system's Vulkan, an offscreen image, PPM dumps).
 *
 * It draws what gui_capi's draw list says, back to front, with one graphics pipeline: a rectangle per operation, whose fragment shader
 * shows a solid colour or reads the pixel of a client's buffer as a storage buffer, one for one (comp.vert, comp.frag). The buffers:
 *   - GPU buffers (GUI_DRAW_GPU): the client's memory, imported where it is (constanos: the opaque-fd import of a /dev/nvgpu BO; host
 *     harness: a descriptor of a memfd, mapped and copied into a buffer of its own before every frame, standing in for shared memory);
 *   - pool windows (GUI_DRAW_CPU): the library's copy of their pixels, uploaded to a host-visible buffer when `version` moves;
 *   - the cursor: a small buffer drawn with a colour key.
 * One frame in flight: a frame starts by waiting for the previous one, so uploads never race the GPU, and then tells the window manager
 * (gui_gpu_frame_done) that the previous frame is done, which is what lets it release buffers.
 */
#ifndef COMP_RENDER_H
#define COMP_RENDER_H

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#ifdef COMP_HOST
#include <sys/mman.h>
#endif

#include "comp_spv.h"
#include "gui_capi.h"

#define COMP_MAX_GPU 64
#define COMP_MAX_CPU 32
#define COMP_MAX_SETS 160

/* Every Vulkan entry point the renderer calls. */
#define COMP_FUNCS(X) \
   X(vkGetDeviceQueue) X(vkCreateBuffer) X(vkDestroyBuffer) X(vkGetBufferMemoryRequirements) X(vkAllocateMemory) X(vkFreeMemory) \
   X(vkBindBufferMemory) X(vkMapMemory) X(vkCreateShaderModule) X(vkDestroyShaderModule) X(vkCreateDescriptorSetLayout) \
   X(vkCreatePipelineLayout) X(vkCreateGraphicsPipelines) X(vkCreateDescriptorPool) X(vkAllocateDescriptorSets) \
   X(vkUpdateDescriptorSets) X(vkFreeDescriptorSets) X(vkCreateCommandPool) X(vkAllocateCommandBuffers) X(vkResetCommandBuffer) \
   X(vkBeginCommandBuffer) X(vkEndCommandBuffer) X(vkCmdPipelineBarrier) X(vkCmdBeginRendering) X(vkCmdEndRendering) \
   X(vkCmdBindPipeline) X(vkCmdBindDescriptorSets) X(vkCmdPushConstants) X(vkCmdSetViewport) X(vkCmdSetScissor) X(vkCmdDraw) \
   X(vkCreateFence) X(vkResetFences) X(vkWaitForFences) X(vkQueueSubmit) X(vkDeviceWaitIdle) \
   X(vkDestroyPipeline) X(vkDestroyPipelineLayout) X(vkDestroyDescriptorSetLayout) X(vkDestroyDescriptorPool) X(vkDestroyCommandPool) X(vkDestroyFence)

struct comp_src {
   uint64_t key;               /* GPU: the handle; CPU: client << 32 | surface; 0 = free */
   VkBuffer buf;
   VkDeviceMemory mem;
   VkDescriptorSet set;
   uint32_t stride_px;
   uint64_t version;           /* CPU: the version uploaded */
   size_t capacity;            /* CPU: bytes the buffer holds */
   uint32_t *map;              /* CPU: mapped; host harness GPU: the buffer's mapping */
#ifdef COMP_HOST
   const uint32_t *shared;     /* host harness GPU: the client's memory (a mapped memfd), copied into `map` every frame */
   size_t shared_bytes;
#endif
   int dropped;                /* GPU: the window manager let go; freed once the frames that may read it are done */
};

struct comp {
   VkDevice device;
   VkPhysicalDeviceMemoryProperties mp;
   uint32_t family;
   VkQueue queue;
#define X(f) PFN_##f f;
   COMP_FUNCS(X)
#undef X
   VkDescriptorSetLayout dsl;
   VkPipelineLayout pl;
   VkPipeline pipe;
   VkDescriptorPool pool;
   VkCommandPool cpool;
   VkCommandBuffer cb;
   VkFence fence;
   int fence_pending;          /* a frame was submitted and its fence has not been waited for */
   uint64_t epoch_pending;     /* its number */
   struct comp_src gpu[COMP_MAX_GPU];
   struct comp_src cpu[COMP_MAX_CPU];
   struct comp_src dummy;      /* what solid fills bind (never read) */
   struct comp_src cursor;
   uint32_t frames, draws, draws_max, imports, drops, uploads;
};

struct comp_push {
   int32_t dst[4];
   int32_t src[4];
   uint32_t misc[4];
};

static int comp_type(const struct comp *c, uint32_t allowed, VkMemoryPropertyFlags want, VkMemoryPropertyFlags avoid) {
   for (uint32_t i = 0; i < c->mp.memoryTypeCount; i++)
      if ((allowed & (1u << i)) && (c->mp.memoryTypes[i].propertyFlags & want) == want && !(c->mp.memoryTypes[i].propertyFlags & avoid))
         return (int)i;
   return -1;
}

static void comp_write_set(struct comp *c, struct comp_src *s) {
   VkDescriptorBufferInfo bi = { .buffer = s->buf, .offset = 0, .range = VK_WHOLE_SIZE };
   VkWriteDescriptorSet w = { .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = s->set, .dstBinding = 0, .descriptorCount = 1,
                              .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .pBufferInfo = &bi };
   c->vkUpdateDescriptorSets(c->device, 1, &w, 0, NULL);
}

static int comp_alloc_set(struct comp *c, struct comp_src *s) {
   VkDescriptorSetAllocateInfo ai = { .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO, .descriptorPool = c->pool, .descriptorSetCount = 1, .pSetLayouts = &c->dsl };
   return c->vkAllocateDescriptorSets(c->device, &ai, &s->set) == VK_SUCCESS ? 0 : -1;
}

/* A host-visible, coherent storage buffer of `bytes` (rounded up to 4096), mapped. */
static int comp_host_buffer(struct comp *c, struct comp_src *s, size_t bytes) {
   bytes = (bytes + 4095) & ~(size_t)4095;
   VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = bytes, .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
   if (c->vkCreateBuffer(c->device, &bci, NULL, &s->buf) != VK_SUCCESS) return -1;
   VkMemoryRequirements req;
   c->vkGetBufferMemoryRequirements(c->device, s->buf, &req);
   int t = comp_type(c, req.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT, 0);
   if (t < 0) return -2;
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = req.size, .memoryTypeIndex = (uint32_t)t };
   if (c->vkAllocateMemory(c->device, &mai, NULL, &s->mem) != VK_SUCCESS) return -3;
   if (c->vkBindBufferMemory(c->device, s->buf, s->mem, 0) != VK_SUCCESS) return -4;
   if (c->vkMapMemory(c->device, s->mem, 0, VK_WHOLE_SIZE, 0, (void **)&s->map) != VK_SUCCESS) return -5;
   s->capacity = bytes;
   return 0;
}

static void comp_free_src(struct comp *c, struct comp_src *s) {
   if (s->set) c->vkFreeDescriptorSets(c->device, c->pool, 1, &s->set);
   if (s->buf) c->vkDestroyBuffer(c->device, s->buf, NULL);
   if (s->mem) c->vkFreeMemory(c->device, s->mem, NULL);
#ifdef COMP_HOST
   if (s->shared) munmap((void *)s->shared, s->shared_bytes);
#endif
   memset(s, 0, sizeof(*s));
}

/* Creates the pipeline and the fixed buffers. `format`: the render target's. 0, or a negative step number. */
static int comp_init(struct comp *c, VkDevice device, PFN_vkGetDeviceProcAddr gdpa, const VkPhysicalDeviceMemoryProperties *mp, uint32_t family, VkFormat format) {
   memset(c, 0, sizeof(*c));
   c->device = device;
   c->mp = *mp;
   c->family = family;
#define X(f) c->f = (PFN_##f)gdpa(device, #f); if (!c->f) { printf("COMP FAIL no %s\n", #f); return -1; }
   COMP_FUNCS(X)
#undef X
   c->vkGetDeviceQueue(device, family, 0, &c->queue);
   VkDescriptorSetLayoutBinding b0 = { .binding = 0, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .descriptorCount = 1, .stageFlags = VK_SHADER_STAGE_FRAGMENT_BIT };
   VkDescriptorSetLayoutCreateInfo dslci = { .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO, .bindingCount = 1, .pBindings = &b0 };
   if (c->vkCreateDescriptorSetLayout(device, &dslci, NULL, &c->dsl) != VK_SUCCESS) return -2;
   VkPushConstantRange pcr = { VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT, 0, sizeof(struct comp_push) };
   VkPipelineLayoutCreateInfo plci = { .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, .setLayoutCount = 1, .pSetLayouts = &c->dsl, .pushConstantRangeCount = 1, .pPushConstantRanges = &pcr };
   if (c->vkCreatePipelineLayout(device, &plci, NULL, &c->pl) != VK_SUCCESS) return -3;
   VkShaderModule vmod, fmod;
   VkShaderModuleCreateInfo vs = { .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = comp_vert_spv_len, .pCode = (const uint32_t *)comp_vert_spv };
   VkShaderModuleCreateInfo fs = { .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = comp_frag_spv_len, .pCode = (const uint32_t *)comp_frag_spv };
   if (c->vkCreateShaderModule(device, &vs, NULL, &vmod) != VK_SUCCESS || c->vkCreateShaderModule(device, &fs, NULL, &fmod) != VK_SUCCESS) return -4;
   VkPipelineShaderStageCreateInfo stages[2] = {
      { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_VERTEX_BIT, .module = vmod, .pName = "main" },
      { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_FRAGMENT_BIT, .module = fmod, .pName = "main" } };
   VkPipelineVertexInputStateCreateInfo vi = { .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO };
   VkPipelineInputAssemblyStateCreateInfo ia = { .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO, .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST };
   VkPipelineViewportStateCreateInfo vp = { .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO, .viewportCount = 1, .scissorCount = 1 };
   VkPipelineRasterizationStateCreateInfo rs = { .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO, .polygonMode = VK_POLYGON_MODE_FILL, .cullMode = VK_CULL_MODE_NONE,
                                                 .frontFace = VK_FRONT_FACE_COUNTER_CLOCKWISE, .lineWidth = 1.0f };
   VkPipelineMultisampleStateCreateInfo ms = { .sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO, .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT };
   VkPipelineColorBlendAttachmentState cba = { .colorWriteMask = VK_COLOR_COMPONENT_R_BIT | VK_COLOR_COMPONENT_G_BIT | VK_COLOR_COMPONENT_B_BIT | VK_COLOR_COMPONENT_A_BIT };
   VkPipelineColorBlendStateCreateInfo cb = { .sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO, .attachmentCount = 1, .pAttachments = &cba };
   VkDynamicState dyn[2] = { VK_DYNAMIC_STATE_VIEWPORT, VK_DYNAMIC_STATE_SCISSOR };
   VkPipelineDynamicStateCreateInfo ds = { .sType = VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO, .dynamicStateCount = 2, .pDynamicStates = dyn };
   VkPipelineRenderingCreateInfo ri = { .sType = VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO, .colorAttachmentCount = 1, .pColorAttachmentFormats = &format };
   VkGraphicsPipelineCreateInfo gp = { .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, .pNext = &ri, .stageCount = 2, .pStages = stages, .pVertexInputState = &vi,
      .pInputAssemblyState = &ia, .pViewportState = &vp, .pRasterizationState = &rs, .pMultisampleState = &ms, .pColorBlendState = &cb, .pDynamicState = &ds, .layout = c->pl };
   if (c->vkCreateGraphicsPipelines(device, VK_NULL_HANDLE, 1, &gp, NULL, &c->pipe) != VK_SUCCESS) return -5;
   c->vkDestroyShaderModule(device, vmod, NULL);
   c->vkDestroyShaderModule(device, fmod, NULL);

   VkDescriptorPoolSize psz = { .type = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .descriptorCount = COMP_MAX_SETS };
   VkDescriptorPoolCreateInfo dpci = { .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO, .flags = VK_DESCRIPTOR_POOL_CREATE_FREE_DESCRIPTOR_SET_BIT, .maxSets = COMP_MAX_SETS, .poolSizeCount = 1, .pPoolSizes = &psz };
   if (c->vkCreateDescriptorPool(device, &dpci, NULL, &c->pool) != VK_SUCCESS) return -6;
   VkCommandPoolCreateInfo cpi = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT, .queueFamilyIndex = family };
   if (c->vkCreateCommandPool(device, &cpi, NULL, &c->cpool) != VK_SUCCESS) return -7;
   VkCommandBufferAllocateInfo cai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = c->cpool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
   if (c->vkAllocateCommandBuffers(device, &cai, &c->cb) != VK_SUCCESS) return -8;
   VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   if (c->vkCreateFence(device, &fci, NULL, &c->fence) != VK_SUCCESS) return -9;

   /* the fixed sources: a page for solid fills to bind, and the cursor (0xFF000000 | colour, 0 = transparent) */
   if (comp_host_buffer(c, &c->dummy, 4096) != 0 || comp_alloc_set(c, &c->dummy) != 0) return -10;
   comp_write_set(c, &c->dummy);
   if (comp_host_buffer(c, &c->cursor, GUI_CURSOR_W * GUI_CURSOR_H * 4) != 0 || comp_alloc_set(c, &c->cursor) != 0) return -11;
   for (int y = 0; y < GUI_CURSOR_H; y++) {
      const char *row = gui_cursor_bitmap((size_t)y);
      for (int x = 0; x < GUI_CURSOR_W; x++)
         c->cursor.map[y * GUI_CURSOR_W + x] = row[x] == 'X' ? 0xff000000u : row[x] == '.' ? 0xffffffffu : 0u;
   }
   c->cursor.stride_px = GUI_CURSOR_W;
   comp_write_set(c, &c->cursor);
   return 0;
}

/* Everything comp_init and the frames made, back to the driver (a device with live objects asserts when destroyed). */
static void comp_destroy(struct comp *c) {
   c->vkDeviceWaitIdle(c->device);
   for (int i = 0; i < COMP_MAX_GPU; i++) if (c->gpu[i].key) comp_free_src(c, &c->gpu[i]);
   for (int i = 0; i < COMP_MAX_CPU; i++) if (c->cpu[i].key) comp_free_src(c, &c->cpu[i]);
   comp_free_src(c, &c->dummy);
   comp_free_src(c, &c->cursor);
   c->vkDestroyFence(c->device, c->fence, NULL);
   c->vkDestroyCommandPool(c->device, c->cpool, NULL);
   c->vkDestroyDescriptorPool(c->device, c->pool, NULL);
   c->vkDestroyPipeline(c->device, c->pipe, NULL);
   c->vkDestroyPipelineLayout(c->device, c->pl, NULL);
   c->vkDestroyDescriptorSetLayout(c->device, c->dsl, NULL);
}

static struct comp_src *comp_find(struct comp_src *tab, int n, uint64_t key) {
   for (int i = 0; i < n; i++) if (tab[i].key == key) return &tab[i];
   return NULL;
}

static struct comp_src *comp_slot(struct comp_src *tab, int n) {
   for (int i = 0; i < n; i++) if (tab[i].key == 0) return &tab[i];
   return NULL;
}

/* A client's GPU buffer, from the window manager's GUI_GPU_IMPORT. Takes the descriptor (closes it). 0, or a negative step. */
static int comp_import(struct comp *c, const struct gui_gpu_op *op) {
   struct comp_src *s = comp_slot(c->gpu, COMP_MAX_GPU);
   if (!s) { close(op->fd); return -1; }
   memset(s, 0, sizeof(*s));
   s->key = op->handle;
   s->stride_px = op->stride / 4;
#ifdef COMP_HOST
   /* the harness: the descriptor is a memfd the "client" wrote its pixels into; map it, and copy it into a buffer of ours every frame */
   void *m = mmap(NULL, op->size, PROT_READ, MAP_SHARED, op->fd, 0);
   close(op->fd);
   if (m == MAP_FAILED) { s->key = 0; return -2; }
   s->shared = m;
   s->shared_bytes = op->size;
   if (comp_host_buffer(c, s, op->size) != 0 || comp_alloc_set(c, s) != 0) { comp_free_src(c, s); return -3; }
#else
   /* constanos: the memory of the client's BO, imported where it is (VRAM), read in place */
   VkExternalMemoryBufferCreateInfo ebi = { .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO, .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT };
   VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .pNext = &ebi, .size = op->size, .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
   if (c->vkCreateBuffer(c->device, &bci, NULL, &s->buf) != VK_SUCCESS) { close(op->fd); comp_free_src(c, s); return -2; }
   VkMemoryRequirements req;
   c->vkGetBufferMemoryRequirements(c->device, s->buf, &req);
   int t = comp_type(c, req.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, 0);
   if (t < 0) t = comp_type(c, req.memoryTypeBits, 0, 0);
   VkImportMemoryFdInfoKHR imp = { .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR, .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT, .fd = op->fd };
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .pNext = &imp, .allocationSize = req.size, .memoryTypeIndex = (uint32_t)(t < 0 ? 0 : t) };
   if (c->vkAllocateMemory(c->device, &mai, NULL, &s->mem) != VK_SUCCESS) { close(op->fd); comp_free_src(c, s); return -3; }   /* the fd is the driver's only on success */
   if (c->vkBindBufferMemory(c->device, s->buf, s->mem, 0) != VK_SUCCESS || comp_alloc_set(c, s) != 0) { comp_free_src(c, s); return -4; }
#endif
   comp_write_set(c, s);
   c->imports++;
   return 0;
}

/* The window manager let go of `handle`: free it once the frames in flight are done (here: the next frame start; one frame is ever in flight). */
static void comp_drop(struct comp *c, uint64_t handle) {
   struct comp_src *s = comp_find(c->gpu, COMP_MAX_GPU, handle);
   if (s) s->dropped = 1;
   c->drops++;
}

static struct comp_src *comp_cpu_source(struct comp *c, const struct gui_draw_op *op, const uint32_t *px, size_t npx) {
   uint64_t key = ((uint64_t)op->client << 32) | op->surface;
   struct comp_src *s = comp_find(c->cpu, COMP_MAX_CPU, key);
   if (s && s->capacity < npx * 4) { comp_free_src(c, s); s = NULL; }   /* the window got bigger */
   if (!s) {
      s = comp_slot(c->cpu, COMP_MAX_CPU);
      if (!s) return NULL;
      memset(s, 0, sizeof(*s));
      if (comp_host_buffer(c, s, npx * 4) != 0 || comp_alloc_set(c, s) != 0) { comp_free_src(c, s); return NULL; }
      s->key = key;
      comp_write_set(c, s);
      s->version = ~0ull;
   }
   if (s->version != op->version || s->stride_px != (uint32_t)op->src_w) {
      memcpy(s->map, px, npx * 4);
      s->version = op->version;
      s->stride_px = (uint32_t)op->src_w;
      c->uploads++;
   }
   return s;
}

/* Waits for the frame in flight and frees what it was reading: tells the window manager its buffers are no longer read. */
static void comp_wait_frame(struct comp *c, gui_comp *g) {
   if (c->fence_pending) {
      c->vkWaitForFences(c->device, 1, &c->fence, VK_TRUE, UINT64_MAX);
      c->fence_pending = 0;
      gui_gpu_frame_done(g, c->epoch_pending);
   }
}

/* One frame: the draw list into `image` (`view`, `w` x `h`, of the `format` given to comp_init), submitted waiting on `wait_sem` (or none) and
 * signalling `signal_sem` (or none), the image left in `final_layout`. 0, or a negative step. */
static int comp_frame(struct comp *c, gui_comp *g, VkImage image, VkImageView view, uint32_t w, uint32_t h, VkSemaphore wait_sem, VkSemaphore signal_sem, VkImageLayout final_layout) {
   comp_wait_frame(c, g);
   struct gui_gpu_op gop;
   while (gui_pop_gpu_op(g, &gop)) {
      if (gop.kind == GUI_GPU_IMPORT) {
         int r = comp_import(c, &gop);
         if (r) printf("COMP the import of buffer %llu failed (%d)\n", (unsigned long long)gop.handle, r);
      } else {
         comp_drop(c, gop.handle);
      }
   }
   int32_t fd;
   while (gui_pop_fd_to_close(g, &fd)) close(fd);
   /* a Drop seen now may name a buffer the previous frame read: that frame is done (waited for above), so it may go */
   for (int i = 0; i < COMP_MAX_GPU; i++)
      if (c->gpu[i].key && c->gpu[i].dropped) comp_free_src(c, &c->gpu[i]);

   uint64_t epoch = gui_draw_list(g);
   const size_t n = gui_draw_count(g);
#ifdef COMP_HOST
   for (int i = 0; i < COMP_MAX_GPU; i++)
      if (c->gpu[i].key && c->gpu[i].shared) memcpy(c->gpu[i].map, c->gpu[i].shared, c->gpu[i].shared_bytes);
#endif
   c->vkResetCommandBuffer(c->cb, 0);
   VkCommandBufferBeginInfo bbi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
   c->vkBeginCommandBuffer(c->cb, &bbi);
   VkImageMemoryBarrier to_color = { .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT, .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
      .newLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .image = image,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
   c->vkCmdPipelineBarrier(c->cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, 0, 0, NULL, 0, NULL, 1, &to_color);
   VkRenderingAttachmentInfo ca = { .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO, .imageView = view, .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
      .loadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE, .storeOp = VK_ATTACHMENT_STORE_OP_STORE };   /* the first operation fills the whole screen */
   VkRenderingInfo ri = { .sType = VK_STRUCTURE_TYPE_RENDERING_INFO, .renderArea = { { 0, 0 }, { w, h } }, .layerCount = 1, .colorAttachmentCount = 1, .pColorAttachments = &ca };
   c->vkCmdBeginRendering(c->cb, &ri);
   VkViewport vp = { 0, 0, (float)w, (float)h, 0, 1 };
   VkRect2D sc = { { 0, 0 }, { w, h } };
   c->vkCmdSetViewport(c->cb, 0, 1, &vp);
   c->vkCmdSetScissor(c->cb, 0, 1, &sc);
   c->vkCmdBindPipeline(c->cb, VK_PIPELINE_BIND_POINT_GRAPHICS, c->pipe);

   uint32_t draws = 0;
   for (size_t i = 0; i < n; i++) {
      struct gui_draw_op op;
      if (gui_draw_get(g, i, &op) != 0) break;
      struct comp_push pc = { { op.x, op.y, op.w, op.h }, { 0, 0, 0, 0 }, { 0, w, h, 0 } };
      struct comp_src *src = &c->dummy;
      switch (op.kind) {
      case GUI_DRAW_FILL:
         pc.misc[0] = op.color;
         break;
      case GUI_DRAW_GPU: {
         struct comp_src *s = comp_find(c->gpu, COMP_MAX_GPU, op.handle);
         if (!s) continue;   /* its import failed: nothing to show */
         src = s;
         pc.src[0] = op.sx; pc.src[1] = op.sy; pc.src[2] = (int32_t)s->stride_px; pc.src[3] = 1;
         break;
      }
      case GUI_DRAW_CPU: {
         size_t len = 0;
         const uint32_t *px = gui_cpu_content(g, op.client, op.surface, &len);
         if (!px) continue;
         struct comp_src *s = comp_cpu_source(c, &op, px, len);
         if (!s) continue;
         src = s;
         pc.src[0] = op.sx; pc.src[1] = op.sy; pc.src[2] = op.src_w; pc.src[3] = 1;
         break;
      }
      case GUI_DRAW_CURSOR:
         /* the whole bitmap at the pointer: what sticks out of the screen is clipped by the viewport, the pixels it hides are never read */
         src = &c->cursor;
         pc.dst[2] = GUI_CURSOR_W; pc.dst[3] = GUI_CURSOR_H;
         pc.src[2] = GUI_CURSOR_W; pc.src[3] = 2;
         break;
      default:
         continue;   /* TITLE: the text is not drawn yet (the bar is) */
      }
      if (pc.dst[2] <= 0 || pc.dst[3] <= 0) continue;
      c->vkCmdBindDescriptorSets(c->cb, VK_PIPELINE_BIND_POINT_GRAPHICS, c->pl, 0, 1, &src->set, 0, NULL);
      c->vkCmdPushConstants(c->cb, c->pl, VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT, 0, sizeof(pc), &pc);
      c->vkCmdDraw(c->cb, 6, 1, 0, 0);
      draws++;
   }
   c->vkCmdEndRendering(c->cb);
   VkImageMemoryBarrier to_final = to_color;
   to_final.srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT;
   to_final.dstAccessMask = final_layout == VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL ? VK_ACCESS_TRANSFER_READ_BIT : 0;
   to_final.oldLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL;
   to_final.newLayout = final_layout;
   c->vkCmdPipelineBarrier(c->cb, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                           final_layout == VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL ? VK_PIPELINE_STAGE_TRANSFER_BIT : VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, NULL, 0, NULL, 1, &to_final);
   c->vkEndCommandBuffer(c->cb);

   VkPipelineStageFlags wait_stage = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
   VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .waitSemaphoreCount = wait_sem ? 1u : 0u, .pWaitSemaphores = &wait_sem, .pWaitDstStageMask = &wait_stage,
      .commandBufferCount = 1, .pCommandBuffers = &c->cb, .signalSemaphoreCount = signal_sem ? 1u : 0u, .pSignalSemaphores = &signal_sem };
   c->vkResetFences(c->device, 1, &c->fence);
   if (c->vkQueueSubmit(c->queue, 1, &si, c->fence) != VK_SUCCESS) return -1;
   c->fence_pending = 1;
   c->epoch_pending = epoch;
   c->frames++;
   c->draws = draws;
   if (draws > c->draws_max) c->draws_max = draws;
   return 0;
}

#endif
