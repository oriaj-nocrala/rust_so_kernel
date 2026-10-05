/*
 * comp_render.h: the GPU compositor's renderer (G5 layer 4, slice 3; docs/gpu/g5-graphics-stack-plan.md). Header-only, shared by
 * vk_comp.c (constanos: NVK linked in, the screen) and the host harness (-DCOMP_HOST: the system's Vulkan, an offscreen image, PPM dumps).
 *
 * It draws a list of `cr_op` (comp_api.h), back to front, with one graphics pipeline: a rectangle per operation, whose fragment shader
 * shows a solid colour, reads the pixel of a client's buffer as a storage buffer, one for one, or computes a shape (rounded box, gradient,
 * border, shadow) from its signed distance (comp.vert, comp.frag). Every draw blends "over" (premultiplied); an opaque one replaces what is
 * under it exactly. The buffers:
 *   - GPU buffers (CR_GPU): the client's memory, imported where it is (constanos: the opaque-fd import of a /dev/nvgpu BO; host
 *     harness: a descriptor of a memfd, mapped and copied into a buffer of its own before every frame, standing in for shared memory);
 *   - pool windows (CR_CPU): the library's copy of their pixels: written to a host-visible staging buffer and copied into a device-local one
 *     (VRAM) when `version` moves, so the fragment shader never reads them over PCIe (Ryzen #193: that cost 4 ms a frame at the lowest P-state);
 *     COMP_CPU_HOST=1 keeps the old way (one host-visible buffer read in place) for comparison, and it is the fallback without a device-local type;
 *   - the cursor: a small buffer drawn with a colour key.
 * One frame in flight: a frame starts by waiting for the previous one, so uploads never race the GPU, and then tells the window manager
 * the caller (comp_wait_frame returns the previous frame's number) that the previous frame is done, which is what lets it release buffers.
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
#include "comp_api.h"

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
   X(vkCmdCopyBuffer) X(vkCreateFence) X(vkResetFences) X(vkWaitForFences) X(vkQueueSubmit) X(vkDeviceWaitIdle) \
   X(vkDestroyPipeline) X(vkDestroyPipelineLayout) X(vkDestroyDescriptorSetLayout) X(vkDestroyDescriptorPool) X(vkDestroyCommandPool) X(vkDestroyFence)

struct comp_src {
   uint64_t key;               /* GPU: the handle; CPU: client << 32 | surface; 0 = free */
   VkBuffer buf;
   VkDeviceMemory mem;
   VkDescriptorSet set;
   uint32_t stride_px;
   uint64_t version;           /* CPU: the version uploaded */
   size_t capacity;            /* CPU: bytes the buffer holds */
   uint32_t *map;              /* CPU: mapped (the staging buffer when `staged`); host harness GPU: the buffer's mapping */
   VkBuffer stage_buf;         /* CPU, `staged`: the host-visible buffer `map` is of; `buf` is then device-local */
   VkDeviceMemory stage_mem;
   int staged;
   uint32_t *shadow;           /* CPU, `staged`: what the staging buffer and VRAM hold (the last version), to find the rows a new version changed */
   uint64_t shadow_npx;
#define COMP_MAX_REGIONS 16
   VkBufferCopy regions[COMP_MAX_REGIONS];   /* CPU, `staged`: the byte ranges written to the staging buffer that the next frame has to copy */
   uint32_t nregions;
#ifdef COMP_HOST
   const uint32_t *shared;     /* host harness GPU: the client's memory (a mapped memfd), copied into `map` every frame */
   size_t shared_bytes;
#endif
   uint32_t used_frame;        /* CPU: the last frame that drew it */
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
   uint32_t frames, draws, draws_max, imports, drops, uploads;
   uint64_t upload_bytes;      /* bytes the frames copied into VRAM (staged sources) or wrote in place (host sources) */
   int cpu_in_host;            /* COMP_CPU_HOST=1: CPU sources stay in one host-visible buffer */
};

/* comp.vert / comp.frag's push constants: 112 bytes (Vulkan guarantees 128) */
struct comp_push {
   int32_t dst[4];
   int32_t src[4];
   uint32_t misc[4];
   int32_t box[4];
   float geom[4];
   uint32_t grad[4];
   uint32_t extra[4];
};

/* comp.frag's mode for a buffer drawn with `alpha` (CR_OPAQUE, CR_KEYED, CR_PREMUL) */
static int32_t comp_source_mode(uint32_t alpha) {
   return alpha == CR_KEYED ? 2 : alpha == CR_PREMUL ? 4 : 1;
}

/* A CR_SHAPE's push constants: the draw covers the box and, with a shadow, the shadow's reach (its offset and blur). */
static void comp_shape_push(const struct cr_op *op, struct comp_push *pc) {
   const struct cr_shape *s = &op->shape;
   int x0 = op->x, y0 = op->y, x1 = op->x + op->w, y1 = op->y + op->h;
   if (s->shadow_color >> 24) {
      int reach = (int)(s->shadow_blur + 1.0f);   /* past the blur's edge the shadow is 0 */
      int sx0 = x0 + s->shadow_dx - reach, sy0 = y0 + s->shadow_dy - reach, sx1 = x1 + s->shadow_dx + reach, sy1 = y1 + s->shadow_dy + reach;
      if (sx0 < x0) x0 = sx0;
      if (sy0 < y0) y0 = sy0;
      if (sx1 > x1) x1 = sx1;
      if (sy1 > y1) y1 = sy1;
   }
   pc->dst[0] = x0; pc->dst[1] = y0; pc->dst[2] = x1 - x0; pc->dst[3] = y1 - y0;
   pc->src[3] = 3;
   pc->misc[3] = s->horizontal ? 1u : 0u;
   pc->box[0] = op->x; pc->box[1] = op->y; pc->box[2] = op->w; pc->box[3] = op->h;
   pc->geom[0] = s->radius; pc->geom[1] = s->border; pc->geom[2] = s->split; pc->geom[3] = s->shadow_blur;
   memcpy(pc->grad, s->c, sizeof(pc->grad));
   pc->extra[0] = s->border_color; pc->extra[1] = s->shadow_color; pc->extra[2] = (uint32_t)s->shadow_dx; pc->extra[3] = (uint32_t)s->shadow_dy;
}

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

/* A buffer of `bytes` (rounded up to 4096) of memory with the `want` properties and none of `avoid`; mapped (into `*map`) when `map` is not NULL. */
static int comp_buffer(struct comp *c, VkBuffer *buf, VkDeviceMemory *mem, uint32_t **map, size_t bytes, VkBufferUsageFlags usage, VkMemoryPropertyFlags want, VkMemoryPropertyFlags avoid) {
   bytes = (bytes + 4095) & ~(size_t)4095;
   VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = bytes, .usage = usage, .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
   if (c->vkCreateBuffer(c->device, &bci, NULL, buf) != VK_SUCCESS) return -1;
   VkMemoryRequirements req;
   c->vkGetBufferMemoryRequirements(c->device, *buf, &req);
   int t = comp_type(c, req.memoryTypeBits, want, avoid);
   if (t < 0) return -2;
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = req.size, .memoryTypeIndex = (uint32_t)t };
   if (c->vkAllocateMemory(c->device, &mai, NULL, mem) != VK_SUCCESS) return -3;
   if (c->vkBindBufferMemory(c->device, *buf, *mem, 0) != VK_SUCCESS) return -4;
   if (map && c->vkMapMemory(c->device, *mem, 0, VK_WHOLE_SIZE, 0, (void **)map) != VK_SUCCESS) return -5;
   return 0;
}

/* A host-visible, coherent storage buffer of `bytes` (rounded up to 4096), mapped. */
static int comp_host_buffer(struct comp *c, struct comp_src *s, size_t bytes) {
   int r = comp_buffer(c, &s->buf, &s->mem, &s->map, bytes, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT, 0);
   if (r == 0) s->capacity = (bytes + 4095) & ~(size_t)4095;
   return r;
}

static void comp_free_src(struct comp *c, struct comp_src *s) {
   if (s->set) c->vkFreeDescriptorSets(c->device, c->pool, 1, &s->set);
   if (s->buf) c->vkDestroyBuffer(c->device, s->buf, NULL);
   if (s->mem) c->vkFreeMemory(c->device, s->mem, NULL);
   if (s->stage_buf) c->vkDestroyBuffer(c->device, s->stage_buf, NULL);
   if (s->stage_mem) c->vkFreeMemory(c->device, s->stage_mem, NULL);
   free(s->shadow);
#ifdef COMP_HOST
   if (s->shared) munmap((void *)s->shared, s->shared_bytes);
#endif
   memset(s, 0, sizeof(*s));
}

/* Creates the pipeline and the fixed buffers. `format`: the render target's. 0, or a negative step number. */
static int comp_init(struct comp *c, VkDevice device, PFN_vkGetDeviceProcAddr gdpa, const VkPhysicalDeviceMemoryProperties *mp, uint32_t family, VkFormat format) {
   memset(c, 0, sizeof(*c));
   c->cpu_in_host = getenv("COMP_CPU_HOST") && getenv("COMP_CPU_HOST")[0] == '1';
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
   /* "over", premultiplied: an opaque draw (alpha 1) writes its colour exactly */
   VkPipelineColorBlendAttachmentState cba = { .blendEnable = VK_TRUE, .srcColorBlendFactor = VK_BLEND_FACTOR_ONE, .dstColorBlendFactor = VK_BLEND_FACTOR_ONE_MINUS_SRC_ALPHA,
      .colorBlendOp = VK_BLEND_OP_ADD, .srcAlphaBlendFactor = VK_BLEND_FACTOR_ONE, .dstAlphaBlendFactor = VK_BLEND_FACTOR_ONE_MINUS_SRC_ALPHA, .alphaBlendOp = VK_BLEND_OP_ADD,
      .colorWriteMask = VK_COLOR_COMPONENT_R_BIT | VK_COLOR_COMPONENT_G_BIT | VK_COLOR_COMPONENT_B_BIT | VK_COLOR_COMPONENT_A_BIT };
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
   return 0;
}

/* Everything comp_init and the frames made, back to the driver (a device with live objects asserts when destroyed). */
static void comp_destroy(struct comp *c) {
   c->vkDeviceWaitIdle(c->device);
   for (int i = 0; i < COMP_MAX_GPU; i++) if (c->gpu[i].key) comp_free_src(c, &c->gpu[i]);
   for (int i = 0; i < COMP_MAX_CPU; i++) if (c->cpu[i].key) comp_free_src(c, &c->cpu[i]);
   comp_free_src(c, &c->dummy);
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

/* A client's GPU buffer (`size` bytes of the descriptor `fd`, rows `stride_bytes` apart), from the window manager's import. Takes the descriptor (closes it). 0, or a negative step. */
static int comp_import(struct comp *c, uint64_t handle, int fd, uint64_t size, uint32_t stride_bytes) {
   struct comp_src *s = comp_slot(c->gpu, COMP_MAX_GPU);
   if (!s) { close(fd); return -1; }
   memset(s, 0, sizeof(*s));
   s->key = handle;
   s->stride_px = stride_bytes / 4;
#ifdef COMP_HOST
   /* the harness: the descriptor is a memfd the "client" wrote its pixels into; map it, and copy it into a buffer of ours every frame */
   void *m = mmap(NULL, size, PROT_READ, MAP_SHARED, fd, 0);
   close(fd);
   if (m == MAP_FAILED) { s->key = 0; return -2; }
   s->shared = m;
   s->shared_bytes = size;
   if (comp_host_buffer(c, s, size) != 0 || comp_alloc_set(c, s) != 0) { comp_free_src(c, s); return -3; }
#else
   /* constanos: the memory of the client's BO, imported where it is (VRAM), read in place */
   VkExternalMemoryBufferCreateInfo ebi = { .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO, .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT };
   VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .pNext = &ebi, .size = size, .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
   if (c->vkCreateBuffer(c->device, &bci, NULL, &s->buf) != VK_SUCCESS) { close(fd); comp_free_src(c, s); return -2; }
   VkMemoryRequirements req;
   c->vkGetBufferMemoryRequirements(c->device, s->buf, &req);
   int t = comp_type(c, req.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, 0);
   if (t < 0) t = comp_type(c, req.memoryTypeBits, 0, 0);
   VkImportMemoryFdInfoKHR imp = { .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR, .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT, .fd = fd };
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .pNext = &imp, .allocationSize = req.size, .memoryTypeIndex = (uint32_t)(t < 0 ? 0 : t) };
   if (c->vkAllocateMemory(c->device, &mai, NULL, &s->mem) != VK_SUCCESS) { close(fd); comp_free_src(c, s); return -3; }   /* the fd is the driver's only on success */
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

/* A new version of a CPU source: writes it into `map` and, for a staged one, plans the copy into VRAM of only the rows that differ from the last version
 * (damage found by comparing: it holds whatever the client said it damaged, and catches a client that damages everything while changing a little).
 * Rows that changed less than COMP_GAP_ROWS apart go in one range; past COMP_MAX_REGIONS ranges the last one grows. A first version, a size change or a
 * second version in the same frame copy everything. */
#define COMP_GAP_ROWS 4
static void comp_plan_upload(struct comp *c, struct comp_src *s, const uint32_t *px, uint64_t npx, uint32_t w) {
   size_t bytes = (size_t)npx * 4;
   if (!s->staged) { memcpy(s->map, px, bytes); c->upload_bytes += bytes; return; }
   if (!s->shadow || s->shadow_npx != npx || s->stride_px != w || w == 0 || s->nregions) {
      if (!s->shadow || s->shadow_npx != npx) { free(s->shadow); s->shadow = malloc(bytes); s->shadow_npx = npx; }
      if (!s->shadow) { s->shadow_npx = 0; memcpy(s->map, px, bytes); s->regions[0] = (VkBufferCopy){ 0, 0, bytes }; s->nregions = 1; c->upload_bytes += bytes; return; }
      memcpy(s->shadow, px, bytes);
      memcpy(s->map, px, bytes);
      s->regions[0] = (VkBufferCopy){ 0, 0, bytes };
      s->nregions = 1;
      c->upload_bytes += bytes;
      return;
   }
   uint64_t rows = (npx + w - 1) / w;
   int64_t r0 = -1, r1 = -1;   /* the range being built, in rows: [r0, r1] */
   for (uint64_t r = 0; r <= rows; r++) {
      int changed = 0;
      if (r < rows) {
         uint64_t off = r * w, len = npx - off < w ? npx - off : w;
         changed = memcmp(s->shadow + off, px + off, (size_t)len * 4) != 0;
      }
      if (changed && r0 < 0) { r0 = (int64_t)r; r1 = (int64_t)r; }
      else if (changed) r1 = (int64_t)r;
      /* a range closes when a gap of unchanged rows is long enough, or at the end */
      if (r0 >= 0 && (r == rows || (!changed && (int64_t)r - r1 > COMP_GAP_ROWS))) {
         uint64_t b0 = (uint64_t)r0 * w, b1 = ((uint64_t)r1 + 1) * w;
         if (b1 > npx) b1 = npx;
         if (s->nregions == COMP_MAX_REGIONS) {   /* out of ranges: the last one grows to cover this one */
            VkBufferCopy *l = &s->regions[COMP_MAX_REGIONS - 1];
            l->size = b1 * 4 - l->srcOffset;
            b0 = l->srcOffset / 4;
         } else {
            s->regions[s->nregions++] = (VkBufferCopy){ b0 * 4, b0 * 4, (b1 - b0) * 4 };
         }
         memcpy(s->shadow + b0, px + b0, (size_t)(b1 - b0) * 4);
         memcpy(s->map + b0, px + b0, (size_t)(b1 - b0) * 4);
         c->upload_bytes += (b1 - b0) * 4;
         r0 = -1;
      }
   }
}

/* The buffer behind a CPU source (a window's pixels, a title, the cursor): made on first use, grown if the source did, uploaded when `version` moves. */
static struct comp_src *comp_cpu_source(struct comp *c, const struct cr_op *op) {
   uint64_t npx = op->npx;
   struct comp_src *s = comp_find(c->cpu, COMP_MAX_CPU, op->key);
   if (s && s->capacity < npx * 4) { comp_free_src(c, s); s = NULL; }   /* the source got bigger */
   if (!s) {
      s = comp_slot(c->cpu, COMP_MAX_CPU);
      if (!s) return NULL;
      memset(s, 0, sizeof(*s));
      /* the pixels live in VRAM and the library writes them into a staging buffer; without a device-local type (or asked to) they stay in host memory */
      size_t bytes = (npx * 4 + 4095) & ~(size_t)4095;
      if (!c->cpu_in_host &&
          comp_buffer(c, &s->stage_buf, &s->stage_mem, &s->map, bytes, VK_BUFFER_USAGE_TRANSFER_SRC_BIT, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT, 0) == 0 &&
          comp_buffer(c, &s->buf, &s->mem, NULL, bytes, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) == 0) {
         s->staged = 1;
         s->capacity = bytes;
      } else {
         comp_free_src(c, s);   /* a half-made pair goes back */
         memset(s, 0, sizeof(*s));
         if (comp_host_buffer(c, s, npx * 4) != 0) { comp_free_src(c, s); return NULL; }
      }
      if (comp_alloc_set(c, s) != 0) { comp_free_src(c, s); return NULL; }
      s->key = op->key;
      comp_write_set(c, s);
      s->version = ~0ull;
   }
   if (s->version != op->version || s->stride_px != (uint32_t)op->src_w) {
      comp_plan_upload(c, s, op->px, npx, (uint32_t)op->src_w);
      s->version = op->version;
      s->stride_px = (uint32_t)op->src_w;
      c->uploads++;
   }
   return s;
}

/* CPU sources nobody drew in the last `COMP_IDLE_FRAMES` frames are freed (a window that closed, a title that changed size): at the end of every frame. */
#define COMP_IDLE_FRAMES 120

/* Waits for the frame in flight. Returns its number (what the caller handed to comp_frame), 0 if none was in flight: that frame is done, so the
 * buffers it read may be released to their clients. */
static uint64_t comp_wait_frame(struct comp *c) {
   uint64_t done = 0;
   if (c->fence_pending) {
      c->vkWaitForFences(c->device, 1, &c->fence, VK_TRUE, UINT64_MAX);
      c->fence_pending = 0;
      done = c->epoch_pending;
   }
   return done;
}

static void comp_free_dropped(struct comp *c) {
   for (int i = 0; i < COMP_MAX_GPU; i++)
      if (c->gpu[i].key && c->gpu[i].dropped) comp_free_src(c, &c->gpu[i]);
}

/* One frame: `ops` (frame number `epoch`) into `image` (`view`, `w` x `h`, of the `format` given to comp_init), submitted waiting on `wait_sem` (or
 * none) and signalling `signal_sem` (or none), the image left in `final_layout`. The previous frame must be waited for already (comp_wait_frame):
 * what it was reading may be freed and rewritten now. 0, or a negative step. */
static int comp_frame(struct comp *c, const struct cr_op *ops, size_t n, uint64_t epoch, VkImage image, VkImageView view, uint32_t w, uint32_t h, VkSemaphore wait_sem, VkSemaphore signal_sem, VkImageLayout final_layout) {
   if (c->fence_pending) return -2;
   comp_free_dropped(c);
#ifdef COMP_HOST
   for (int i = 0; i < COMP_MAX_GPU; i++)
      if (c->gpu[i].key && c->gpu[i].shared) memcpy(c->gpu[i].map, c->gpu[i].shared, c->gpu[i].shared_bytes);
#endif
   c->vkResetCommandBuffer(c->cb, 0);
   VkCommandBufferBeginInfo bbi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
   c->vkBeginCommandBuffer(c->cb, &bbi);
   /* the CPU sources first (a copy cannot be inside the render pass): new versions go from the staging buffers into VRAM */
   int copied = 0;
   for (size_t i = 0; i < n; i++) {
      if (ops[i].kind != CR_CPU || !ops[i].px) continue;
      struct comp_src *s = comp_cpu_source(c, &ops[i]);
      if (s && s->staged && s->nregions) {
         c->vkCmdCopyBuffer(c->cb, s->stage_buf, s->buf, s->nregions, s->regions);
         s->nregions = 0;
         copied = 1;
      }
   }
   if (copied) {
      VkMemoryBarrier mb = { .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER, .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT, .dstAccessMask = VK_ACCESS_SHADER_READ_BIT };
      c->vkCmdPipelineBarrier(c->cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, 0, 1, &mb, 0, NULL, 0, NULL);
   }
   VkImageMemoryBarrier to_color = { .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT, .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
      .newLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .image = image,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
   c->vkCmdPipelineBarrier(c->cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, 0, 0, NULL, 0, NULL, 1, &to_color);
   VkRenderingAttachmentInfo ca = { .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO, .imageView = view, .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
      .loadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE, .storeOp = VK_ATTACHMENT_STORE_OP_STORE };   /* the first operation fills the whole screen, opaque */
   VkRenderingInfo ri = { .sType = VK_STRUCTURE_TYPE_RENDERING_INFO, .renderArea = { { 0, 0 }, { w, h } }, .layerCount = 1, .colorAttachmentCount = 1, .pColorAttachments = &ca };
   c->vkCmdBeginRendering(c->cb, &ri);
   VkViewport vp = { 0, 0, (float)w, (float)h, 0, 1 };
   VkRect2D sc = { { 0, 0 }, { w, h } };
   c->vkCmdSetViewport(c->cb, 0, 1, &vp);
   c->vkCmdSetScissor(c->cb, 0, 1, &sc);
   c->vkCmdBindPipeline(c->cb, VK_PIPELINE_BIND_POINT_GRAPHICS, c->pipe);

   uint32_t draws = 0;
   for (size_t i = 0; i < n; i++) {
      const struct cr_op *op = &ops[i];
      struct comp_push pc = { .dst = { op->x, op->y, op->w, op->h }, .misc = { 0, w, h, 0 } };
      struct comp_src *src = &c->dummy;
      switch (op->kind) {
      case CR_FILL:
         pc.misc[0] = op->color;
         break;
      case CR_GPU: {
         struct comp_src *s = comp_find(c->gpu, COMP_MAX_GPU, op->key);
         if (!s) continue;   /* its import failed: nothing to show */
         src = s;
         pc.src[0] = op->sx; pc.src[1] = op->sy; pc.src[2] = (int32_t)s->stride_px; pc.src[3] = comp_source_mode(op->alpha);
         break;
      }
      case CR_CPU: {
         if (!op->px) continue;
         struct comp_src *s = comp_cpu_source(c, op);
         if (!s) continue;
         s->used_frame = c->frames;
         src = s;
         pc.src[0] = op->sx; pc.src[1] = op->sy; pc.src[2] = op->src_w; pc.src[3] = comp_source_mode(op->alpha);
         break;
      }
      case CR_SHAPE:
         if (op->w <= 0 || op->h <= 0) continue;
         comp_shape_push(op, &pc);
         break;
      default:
         continue;
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
   /* sources nobody drew for a long while (a closed window, a title that changed size) go back; the frame in flight only reads the ones it drew */
   for (int i = 0; i < COMP_MAX_CPU; i++)
      if (c->cpu[i].key && c->frames - c->cpu[i].used_frame > COMP_IDLE_FRAMES) comp_free_src(c, &c->cpu[i]);
   c->draws = draws;
   if (draws > c->draws_max) c->draws_max = draws;
   return 0;
}

#endif
