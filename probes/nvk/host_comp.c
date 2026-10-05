/*
 * host_comp: the GPU compositor's renderer (comp_render.h) driven on the host's own Vulkan with fake clients, against a CPU reference
 * (G5 layer 4, slice 3). `probes/nvk/host-comp.sh [dump-dir]` builds and runs it. The window manager is the real one (gui_capi), the clients are
 * in-process: a "GPU" client whose buffer is a memfd it writes pixels into (the harness's stand-in for a client's VRAM buffer), and a
 * pool client (shm) for the CPU path. Each frame is rendered into an offscreen image, read back, and compared pixel for pixel with a
 * rasterisation of the same draw list on the CPU (titles are not drawn by either). Exit 1 on the first difference.
 * Some frames add operations the window manager does not make yet (shapes, premultiplied pixels: docs/gui/compositor-visual-plan.md):
 * those are computed in floats on both sides, so they compare within `tol` per channel, and the reference itself is checked on known pixels.
 */
#define _GNU_SOURCE
#include <vulkan/vulkan.h>

#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#include "comp_render.h"
#include "constanos_gui_wire.h"
#include "gui_capi.h"

#define W 640
#define H 360

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("ok   %s\n", #cond); else { failures++; printf("FAIL %s (line %d): ", #cond, __LINE__); printf(__VA_ARGS__); printf("\n"); } } while (0)

static gui_comp *g;

struct client {
   uint32_t id;
   int fd;           /* the memfd the pixels live in */
   uint32_t *px;     /* its mapping (the client's side) */
   size_t bytes;
};

static void send_out(uint32_t client, struct guiw_out *o) {
   gui_client_data(g, client, o->bytes, o->len, o->fds, (size_t)o->nfds);
   memset(o, 0, sizeof(*o));
}

/* Events queued for `client` as (object, opcode); other clients' are dropped. */
static int has_release(uint32_t client, uint32_t buffer) {
   int found = 0;
   uint8_t buf[4096];
   uint32_t who;
   size_t len;
   while ((len = gui_pop_event(g, &who, buf, sizeof(buf))) > 0) {
      struct guiw_msg m = {0};
      if (guiw_next(buf, len, &m) > 0 && who == client && m.object == buffer && m.opcode == GUIW_EV_RELEASE) found = 1;
   }
   return found;
}

static uint32_t gradient(int x, int y, int seed) {
   return ((uint32_t)((x * 255 / 200 + seed * 40) & 255) << 16) | ((uint32_t)((y * 255 / 120 + seed * 70) & 255) << 8) | (uint32_t)((x ^ y) & 255);
}

/* A client with a surface showing a GPU buffer w x h (buffer id `bid`, its pixels from `gradient(.., seed)`). */
static struct client gpu_window(int w, int h, uint32_t bid, int seed, const char *title) {
   struct client c = { .id = gui_add_client(g) };
   c.bytes = (size_t)w * h * 4;
   c.fd = memfd_create("gpu-buffer", 0);
   if (c.fd < 0 || ftruncate(c.fd, (off_t)c.bytes) != 0) { printf("FAIL memfd\n"); exit(1); }
   c.px = mmap(NULL, c.bytes, PROT_READ | PROT_WRITE, MAP_SHARED, c.fd, 0);
   for (int y = 0; y < h; y++) for (int x = 0; x < w; x++) c.px[y * w + x] = gradient(x, y, seed);
   struct guiw_out o;
   memset(&o, 0, sizeof(o));
   guiw_create_surface(&o, 2);
   guiw_set_title(&o, 2, title);
   guiw_create_gpu_buffer(&o, bid, dup(c.fd), (uint32_t)c.bytes, w, h, w * 4, GUIW_FORMAT_XRGB8888);
   guiw_attach(&o, 2, bid);
   guiw_commit(&o, 2);
   send_out(c.id, &o);
   return c;
}

static struct client pool_window(int w, int h, uint32_t color) {
   struct client c = { .id = gui_add_client(g) };
   c.bytes = (size_t)w * h * 4;
   c.fd = memfd_create("pool", 0);
   if (c.fd < 0 || ftruncate(c.fd, (off_t)c.bytes) != 0) { printf("FAIL memfd\n"); exit(1); }
   c.px = mmap(NULL, c.bytes, PROT_READ | PROT_WRITE, MAP_SHARED, c.fd, 0);
   for (int y = 0; y < h; y++) for (int x = 0; x < w; x++) c.px[y * w + x] = ((x / 8 + y / 8) & 1) ? color : (color ^ 0x00404040u);
   struct guiw_out o;
   memset(&o, 0, sizeof(o));
   guiw_create_pool(&o, 2, dup(c.fd), (uint32_t)c.bytes);
   guiw_create_buffer(&o, 2, 3, 0, w, h, w * 4, GUIW_FORMAT_XRGB8888);
   guiw_create_surface(&o, 4);
   guiw_set_title(&o, 4, "pool");
   guiw_attach(&o, 4, 3);
   guiw_commit(&o, 4);
   send_out(c.id, &o);
   return c;
}

/* Operations drawn after the window manager's (on top of everything), and how far (per channel) the GPU may be from the reference. */
static const struct cr_op *extra_ops;
static size_t n_extra;
static int tol;

static float clampf(float x, float lo, float hi) { return x < lo ? lo : x > hi ? hi : x; }

/* 0xAARRGGBB straight -> premultiplied, as comp.frag's unpack_straight */
static void unpack_straight(uint32_t v, float o[4]) {
   float a = (float)(v >> 24) / 255.0f;
   o[0] = (float)((v >> 16) & 255) / 255.0f * a;
   o[1] = (float)((v >> 8) & 255) / 255.0f * a;
   o[2] = (float)(v & 255) / 255.0f * a;
   o[3] = a;
}

static float sd_round_box(float px, float py, float bx, float by, float r) {
   float qx = fabsf(px) - bx + r, qy = fabsf(py) - by + r;
   float mx = fmaxf(qx, 0.0f), my = fmaxf(qy, 0.0f);
   return sqrtf(mx * mx + my * my) + fminf(fmaxf(qx, qy), 0.0f) - r;
}

static float smoothstepf(float e0, float e1, float x) {
   float t = clampf((x - e0) / (e1 - e0), 0.0f, 1.0f);
   return t * t * (3.0f - 2.0f * t);
}

static void mixv(const float a[4], const float b[4], float t, float o[4]) {
   for (int i = 0; i < 4; i++) o[i] = a[i] * (1.0f - t) + b[i] * t;
}

/* A CR_SHAPE's premultiplied colour at the pixel (x, y), as comp.frag's shape() */
static void shape_pixel(const struct cr_op *op, int x, int y, float o[4]) {
   const struct cr_shape *s = &op->shape;
   float px = (float)x + 0.5f, py = (float)y + 0.5f;
   float bx = (float)op->w * 0.5f, by = (float)op->h * 0.5f;
   float cx = (float)op->x + bx, cy = (float)op->y + by;
   float r = fminf(s->radius, fminf(bx, by));
   float d = sd_round_box(px - cx, py - cy, bx, by, r);
   float cover = clampf(0.5f - d, 0.0f, 1.0f);
   float t = s->horizontal ? (px - (float)op->x) / (float)op->w : (py - (float)op->y) / (float)op->h;
   t = clampf(t, 0.0f, 1.0f);
   float c[4][4], fill[4];
   for (int i = 0; i < 4; i++) unpack_straight(s->c[i], c[i]);
   if (t < s->split) mixv(c[0], c[1], t / s->split, fill);
   else mixv(c[2], c[3], s->split < 1.0f ? (t - s->split) / (1.0f - s->split) : 1.0f, fill);
   if (s->border > 0.0f) {
      float bc[4];
      unpack_straight(s->border_color, bc);
      mixv(bc, fill, clampf(0.5f - (d + s->border), 0.0f, 1.0f), fill);
   }
   for (int i = 0; i < 4; i++) o[i] = fill[i] * cover;
   float sc[4];
   unpack_straight(s->shadow_color, sc);
   if (sc[3] > 0.0f) {
      float ds = sd_round_box(px - cx - (float)s->shadow_dx, py - cy - (float)s->shadow_dy, bx, by, r);
      float k = s->shadow_blur > 0.0f ? 1.0f - smoothstepf(-s->shadow_blur, s->shadow_blur, ds) : clampf(0.5f - ds, 0.0f, 1.0f);
      for (int i = 0; i < 4; i++) o[i] += sc[i] * (k * (1.0f - cover));
   }
}

/* `src` (premultiplied, 0..1) over the pixel at `dst`, rounded as a UNORM attachment stores it */
static void blend_over(uint32_t *dst, const float src[4]) {
   uint32_t out = 0;
   for (int i = 0; i < 3; i++) {
      int sh = 16 - 8 * i;
      float d = (float)((*dst >> sh) & 255) / 255.0f;
      out |= (uint32_t)lrintf(clampf(src[i] + d * (1.0f - src[3]), 0.0f, 1.0f) * 255.0f) << sh;
   }
   *dst = out;
}

/* every pixel of the screen: a renderer that covers too little (the shadow's reach) shows as a difference */
static void reference_shape(const struct cr_op *op, uint32_t *out) {
   for (int y = 0; y < H; y++)
      for (int x = 0; x < W; x++) {
         float c[4];
         shape_pixel(op, x, y, c);
         if (c[3] > 0.0f) blend_over(&out[y * W + x], c);
      }
}

/* A GUI_DRAW_SHAPE as the renderer's operation (what vk-comp/src/lib.rs does with a DrawOp::Shape) */
static struct cr_op shape_op(const struct gui_draw_op *d) {
   struct cr_op o = { .kind = CR_SHAPE, .x = d->x, .y = d->y, .w = d->w, .h = d->h };
   o.shape = (struct cr_shape){ .radius = d->shape_radius, .border = d->shape_border, .split = d->shape_split, .shadow_blur = d->shape_shadow_blur,
      .border_color = d->shape_border_color, .shadow_color = d->shape_shadow_color, .shadow_dx = d->shape_shadow_dx, .shadow_dy = d->shape_shadow_dy,
      .horizontal = d->shape_horizontal };
   memcpy(o.shape.c, d->shape_c, sizeof(o.shape.c));
   return o;
}

static void reference_extra(uint32_t *out) {
   for (size_t i = 0; i < n_extra; i++) {
      const struct cr_op *op = &extra_ops[i];
      if (op->kind == CR_SHAPE) {
         reference_shape(op, out);
      } else if (op->kind == CR_CPU && op->alpha == CR_PREMUL) {
         for (int y = 0; y < op->h; y++)
            for (int x = 0; x < op->w; x++) {
               uint32_t v = op->px[(op->sy + y) * op->src_w + op->sx + x];
               float c[4] = { (float)((v >> 16) & 255) / 255.0f, (float)((v >> 8) & 255) / 255.0f, (float)(v & 255) / 255.0f, (float)(v >> 24) / 255.0f };
               blend_over(&out[(op->y + y) * W + op->x + x], c);
            }
      }
   }
}

/* The draw list last built, rasterised on the CPU. */
static void reference(struct comp *c, uint32_t *out) {
   const size_t n = gui_draw_count(g);
   for (size_t i = 0; i < n; i++) {
      struct gui_draw_op op;
      gui_draw_get(g, i, &op);
      switch (op.kind) {
      case GUI_DRAW_FILL:
         for (int y = op.y; y < op.y + op.h; y++) for (int x = op.x; x < op.x + op.w; x++) out[y * W + x] = op.color;
         break;
      case GUI_DRAW_GPU: {
         struct comp_src *s = comp_find(c->gpu, COMP_MAX_GPU, op.handle);
         if (!s) break;
         for (int y = 0; y < op.h; y++) for (int x = 0; x < op.w; x++) out[(op.y + y) * W + op.x + x] = s->shared[(op.sy + y) * s->stride_px + op.sx + x];
         break;
      }
      case GUI_DRAW_CPU: {
         size_t len;
         const uint32_t *px = gui_cpu_content(g, op.client, op.surface, &len);
         for (int y = 0; y < op.h; y++) for (int x = 0; x < op.w; x++) out[(op.y + y) * W + op.x + x] = px[(op.sy + y) * op.src_w + op.sx + x];
         break;
      }
      case GUI_DRAW_SHAPE: {
         struct cr_op so = shape_op(&op);
         reference_shape(&so, out);
         break;
      }
      case GUI_DRAW_CURSOR:
         for (int y = 0; y < GUI_CURSOR_H; y++) {
            for (int x = 0; x < GUI_CURSOR_W; x++) {
               int px = op.x + x, py = op.y + y;
               char ch = gui_cursor_bitmap((size_t)y)[x];
               if (px < 0 || py < 0 || px >= W || py >= H || ch == ' ') continue;
               out[py * W + px] = ch == 'X' ? 0 : 0x00ffffffu;
            }
         }
         break;
      default: break;
      }
   }
}

struct vkctx {
   VkInstance instance;
   VkDevice device;
   VkPhysicalDevice pdev;
   uint32_t family;
   VkImage image;
   VkDeviceMemory image_mem;
   VkImageView view;
   VkBuffer readback;
   VkDeviceMemory readback_mem;
   uint32_t *rb_map;
   PFN_vkGetDeviceProcAddr gdpa;
};

static void dump_ppm(const char *dir, const char *name, const uint32_t *px) {
   if (!dir) return;
   char path[512];
   snprintf(path, sizeof(path), "%s/%s.ppm", dir, name);
   FILE *f = fopen(path, "wb");
   if (!f) return;
   fprintf(f, "P6\n%d %d\n255\n", W, H);
   for (int i = 0; i < W * H; i++) { uint8_t rgb[3] = { (uint8_t)(px[i] >> 16), (uint8_t)(px[i] >> 8), (uint8_t)px[i] }; fwrite(rgb, 1, 3, f); }
   fclose(f);
}

/* One frame through the renderer, read back, compared with the reference. */
static int frame(struct vkctx *v, struct comp *c, const char *dir, const char *name) {
   VkCommandPool pool;
   VkCommandBuffer cb;
   PFN_vkCreateCommandPool ccp = (PFN_vkCreateCommandPool)v->gdpa(v->device, "vkCreateCommandPool");
   PFN_vkAllocateCommandBuffers acb = (PFN_vkAllocateCommandBuffers)v->gdpa(v->device, "vkAllocateCommandBuffers");
   (void)ccp; (void)acb; (void)pool; (void)cb;
   /* what the compositor program does each frame, in C: the previous frame is done -> the window manager; its imports and drops -> the renderer;
    * its draw list -> the renderer's operations (the cursor is a keyed pixel source) */
   uint64_t done = comp_wait_frame(c);
   if (done) gui_gpu_frame_done(g, done);
   struct gui_gpu_op gop;
   while (gui_pop_gpu_op(g, &gop)) {
      if (gop.kind == GUI_GPU_IMPORT) { if (comp_import(c, gop.handle, gop.fd, gop.size, gop.stride)) printf("FAIL import\n"); }
      else comp_drop(c, gop.handle);
   }
   int32_t cfd;
   while (gui_pop_fd_to_close(g, &cfd)) close(cfd);
   uint64_t epoch = gui_draw_list(g);
   static uint32_t cursor_px[GUI_CURSOR_W * GUI_CURSOR_H];
   for (int y = 0; y < GUI_CURSOR_H; y++)
      for (int x = 0; x < GUI_CURSOR_W; x++) {
         char ch = gui_cursor_bitmap((size_t)y)[x];
         cursor_px[y * GUI_CURSOR_W + x] = ch == 'X' ? 0xff000000u : ch == '.' ? 0xffffffffu : 0u;
      }
   size_t nops = gui_draw_count(g);
   struct cr_op *ops = calloc(nops + n_extra + 1, sizeof(*ops));
   size_t no = 0;
   for (size_t i = 0; i < nops; i++) {
      struct gui_draw_op d;
      gui_draw_get(g, i, &d);
      struct cr_op *o = &ops[no];
      switch (d.kind) {
      case GUI_DRAW_SHAPE: *o = shape_op(&d); no++; break;
      case GUI_DRAW_FILL: *o = (struct cr_op){ .kind = CR_FILL, .color = d.color, .x = d.x, .y = d.y, .w = d.w, .h = d.h }; no++; break;
      case GUI_DRAW_GPU: *o = (struct cr_op){ .kind = CR_GPU, .key = d.handle, .x = d.x, .y = d.y, .w = d.w, .h = d.h, .sx = d.sx, .sy = d.sy }; no++; break;
      case GUI_DRAW_CPU: {
         size_t len = 0;
         const uint32_t *px = gui_cpu_content(g, d.client, d.surface, &len);
         *o = (struct cr_op){ .kind = CR_CPU, .key = ((uint64_t)d.client << 32) | d.surface, .version = d.version, .px = px, .npx = len, .src_w = d.src_w,
                              .x = d.x, .y = d.y, .w = d.w, .h = d.h, .sx = d.sx, .sy = d.sy };
         no++;
         break;
      }
      case GUI_DRAW_CURSOR:
         *o = (struct cr_op){ .kind = CR_CPU, .key = 1ull << 63, .version = 1, .px = cursor_px, .npx = GUI_CURSOR_W * GUI_CURSOR_H, .src_w = GUI_CURSOR_W, .alpha = CR_KEYED,
                              .x = d.x, .y = d.y, .w = GUI_CURSOR_W, .h = GUI_CURSOR_H };
         no++;
         break;
      default: break;
      }
   }
   for (size_t i = 0; i < n_extra; i++) ops[no++] = extra_ops[i];
   int r = comp_frame(c, ops, no, epoch, v->image, v->view, W, H, VK_NULL_HANDLE, VK_NULL_HANDLE, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL);
   free(ops);
   if (r != 0) { printf("FAIL comp_frame -> %d\n", r); failures++; return -1; }
   /* the readback: a one-off command buffer copying the image to the host-visible buffer */
   VkCommandPoolCreateInfo cpi = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = v->family };
   VkCommandPool rp;
   c->vkCreateCommandPool(v->device, &cpi, NULL, &rp);
   VkCommandBufferAllocateInfo cai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = rp, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
   VkCommandBuffer rcb;
   c->vkAllocateCommandBuffers(v->device, &cai, &rcb);
   VkCommandBufferBeginInfo bbi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
   c->vkBeginCommandBuffer(rcb, &bbi);
   PFN_vkCmdCopyImageToBuffer copy = (PFN_vkCmdCopyImageToBuffer)v->gdpa(v->device, "vkCmdCopyImageToBuffer");
   VkBufferImageCopy bic = { .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 }, .imageExtent = { W, H, 1 } };
   copy(rcb, v->image, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, v->readback, 1, &bic);
   c->vkEndCommandBuffer(rcb);
   VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &rcb };
   c->vkQueueSubmit(c->queue, 1, &si, VK_NULL_HANDLE);
   c->vkDeviceWaitIdle(v->device);
   PFN_vkDestroyCommandPool dcp = (PFN_vkDestroyCommandPool)v->gdpa(v->device, "vkDestroyCommandPool");
   dcp(v->device, rp, NULL);

   static uint32_t want[W * H];
   for (int i = 0; i < W * H; i++) want[i] = 0xdeadbeefu;
   reference(c, want);
   reference_extra(want);
   int bad = 0, bx = -1, by = -1, worst = 0;
   for (int y = 0; y < H; y++) {
      for (int x = 0; x < W; x++) {
         uint32_t got = v->rb_map[y * W + x], w = want[y * W + x];
         int diff = 0;
         for (int sh = 0; sh < 24; sh += 8) {
            int d = abs((int)((got >> sh) & 255) - (int)((w >> sh) & 255));
            if (d > diff) diff = d;
         }
         if (diff > worst) worst = diff;
         if (diff > tol && !bad++) { bx = x; by = y; }
      }
   }
   dump_ppm(dir, name, v->rb_map);
   printf("frame %s: %u draws, %u uploads, %d pixels differ (by up to %d, %d allowed)", name, c->draws, c->uploads, bad, worst, tol);
   if (bad) printf(" (first at %d,%d: got %06x want %06x)", bx, by, v->rb_map[by * W + bx] & 0xffffff, want[by * W + bx] & 0xffffff);
   printf("\n");
   return bad;
}

int main(int argc, char **argv) {
   setvbuf(stdout, NULL, _IONBF, 0);
   const char *dir = argc > 1 ? argv[1] : NULL;
   struct vkctx v;
   memset(&v, 0, sizeof(v));
   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .pApplicationName = "host_comp", .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app };
   if (vkCreateInstance(&ici, NULL, &v.instance) != VK_SUCCESS) { printf("FAIL no Vulkan\n"); return 1; }
   uint32_t n = 8;
   VkPhysicalDevice pds[8];
   vkEnumeratePhysicalDevices(v.instance, &n, pds);
   if (n == 0) { printf("FAIL no physical device\n"); return 1; }
   v.pdev = pds[0];
   for (uint32_t i = 0; i < n; i++) {
      VkPhysicalDeviceProperties pr;
      vkGetPhysicalDeviceProperties(pds[i], &pr);
      if (pr.deviceType == VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU) { v.pdev = pds[i]; break; }
   }
   VkPhysicalDeviceProperties pr;
   vkGetPhysicalDeviceProperties(v.pdev, &pr);
   printf("host_comp: %s\n", pr.deviceName);
   uint32_t nq = 16;
   VkQueueFamilyProperties qf[16];
   vkGetPhysicalDeviceQueueFamilyProperties(v.pdev, &nq, qf);
   int family = -1;
   for (uint32_t i = 0; i < nq && family < 0; i++) if (qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) family = (int)i;
   v.family = (uint32_t)family;
   float prio = 1;
   VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = v.family, .queueCount = 1, .pQueuePriorities = &prio };
   VkPhysicalDeviceVulkan13Features f13 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES, .dynamicRendering = VK_TRUE };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &f13, .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci };
   if (vkCreateDevice(v.pdev, &dci, NULL, &v.device) != VK_SUCCESS) { printf("FAIL device\n"); return 1; }
   v.gdpa = (PFN_vkGetDeviceProcAddr)vkGetInstanceProcAddr(v.instance, "vkGetDeviceProcAddr");
   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(v.pdev, &mp);

   struct comp comp;
   int r = comp_init(&comp, v.device, v.gdpa, &mp, v.family, VK_FORMAT_B8G8R8A8_UNORM);
   CHECK(r == 0, "the renderer starts (step %d)", r);
   if (r) return 1;
   PFN_vkCreateImage ci = (PFN_vkCreateImage)v.gdpa(v.device, "vkCreateImage");
   PFN_vkGetImageMemoryRequirements gmr = (PFN_vkGetImageMemoryRequirements)v.gdpa(v.device, "vkGetImageMemoryRequirements");
   PFN_vkBindImageMemory bim = (PFN_vkBindImageMemory)v.gdpa(v.device, "vkBindImageMemory");
   PFN_vkCreateImageView civ = (PFN_vkCreateImageView)v.gdpa(v.device, "vkCreateImageView");
   VkImageCreateInfo imi = { .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .imageType = VK_IMAGE_TYPE_2D, .format = VK_FORMAT_B8G8R8A8_UNORM, .extent = { W, H, 1 }, .mipLevels = 1, .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_OPTIMAL, .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT, .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED };
   ci(v.device, &imi, NULL, &v.image);
   VkMemoryRequirements req;
   gmr(v.device, v.image, &req);
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = req.size, .memoryTypeIndex = (uint32_t)comp_type(&comp, req.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, 0) };
   comp.vkAllocateMemory(v.device, &mai, NULL, &v.image_mem);
   bim(v.device, v.image, v.image_mem, 0);
   VkImageViewCreateInfo ivci = { .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .image = v.image, .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = VK_FORMAT_B8G8R8A8_UNORM,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
   civ(v.device, &ivci, NULL, &v.view);
   struct comp_src rb = {0};
   CHECK(comp_host_buffer(&comp, &rb, (size_t)W * H * 4) == 0, "a readback buffer");
   v.readback = rb.buf;
   v.rb_map = rb.map;

   g = gui_new(W, H);
   gui_enable_gpu_buffers(g);

   /* ---- frame 1: a GPU window and a pool window on top of it, the cursor */
   struct client a = gpu_window(200, 120, 3, 0, "gpu window");
   struct client b = pool_window(160, 90, 0x00c04020u);
   gui_set_time(g, 100);
   failures += frame(&v, &comp, dir, "1-two-windows") != 0;
   CHECK(comp.imports == 1, "the GPU buffer was imported once (%u)", comp.imports);

   /* ---- frame 2: the GPU client moves to a second buffer; the first is released once a frame that read it is done */
   struct guiw_out o;
   memset(&o, 0, sizeof(o));
   uint32_t *second = NULL;
   int fd2 = memfd_create("gpu-buffer-2", 0);
   ftruncate(fd2, 200 * 120 * 4);
   second = mmap(NULL, 200 * 120 * 4, PROT_READ | PROT_WRITE, MAP_SHARED, fd2, 0);
   for (int y = 0; y < 120; y++) for (int x = 0; x < 200; x++) second[y * 200 + x] = gradient(x, y, 3);
   guiw_create_gpu_buffer(&o, 5, dup(fd2), 200 * 120 * 4, 200, 120, 800, GUIW_FORMAT_XRGB8888);
   guiw_attach(&o, 2, 5);
   guiw_commit(&o, 2);
   send_out(a.id, &o);
   CHECK(!has_release(a.id, 3), "buffer 3 is not released at the commit: frame 1 read it");
   failures += frame(&v, &comp, dir, "2-second-buffer") != 0;
   /* the frame after waits for frame 2, which tells the window manager frame 1 (and 2) are done */
   failures += frame(&v, &comp, dir, "3-idle") != 0;
   CHECK(has_release(a.id, 3), "buffer 3 was released once the frames that read it were done");
   CHECK(comp.imports == 2, "the second buffer was imported (%u)", comp.imports);

   /* ---- frame 4: the pool window changes, the pointer moves, the GPU window is dragged by its title bar past the left edge */
   for (int i = 0; i < 160 * 90; i++) b.px[i] ^= 0x00ffffffu;
   memset(&o, 0, sizeof(o));
   guiw_attach(&o, 4, 3);
   guiw_damage(&o, 4, 0, 0, 160, 90);
   guiw_commit(&o, 4);
   send_out(b.id, &o);
   gui_set_time(g, 200);
   /* grab a's title bar: a was mapped first, so it is at (40, 40); b above it at a cascade offset */
   gui_pointer_motion(g, 50 - W / 2, 45 - H / 2);
   gui_pointer_button(g, 0x110, 1);
   gui_pointer_motion(g, -120, 30);
   gui_pointer_button(g, 0x110, 0);
   unsigned up_before = comp.uploads;
   failures += frame(&v, &comp, dir, "4-dragged") != 0;
   CHECK(comp.uploads > up_before, "the pool window's new pixels were uploaded (%u)", comp.uploads);
   unsigned up2 = comp.uploads;
   failures += frame(&v, &comp, dir, "5-unchanged") != 0;
   CHECK(comp.uploads == up2, "nothing is uploaded when nothing changed");

   /* ---- a few rows of the pool window change: only those rows are copied into VRAM, and the picture is still exact (the reference compares every pixel) */
   {
      for (int x = 0; x < 160; x++) { b.px[20 * 160 + x] ^= 0x00ff00ffu; b.px[60 * 160 + x] ^= 0x0000ffffu; b.px[61 * 160 + x] ^= 0x00ffff00u; }
      memset(&o, 0, sizeof(o));
      guiw_attach(&o, 4, 3);
      guiw_damage(&o, 4, 0, 0, 160, 90);   /* the client says everything: the renderer finds the three rows */
      guiw_commit(&o, 4);
      send_out(b.id, &o);
      gui_set_time(g, 230);
      uint64_t before = comp.upload_bytes;
      failures += frame(&v, &comp, dir, "5b-three-rows") != 0;
      /* a source is staged (only the changed rows copied into VRAM) when the device has memory that is device-local and not host-visible;
       * lavapipe has none, so there every version is written whole into a host-visible buffer */
      struct comp_src *ps = comp_find(comp.cpu, COMP_MAX_CPU, ((uint64_t)b.id << 32) | 4);
      CHECK(ps != NULL, "the pool window has a CPU source");
      CHECK(comp.upload_bytes - before == (!ps || !ps->staged ? 160u * 90 * 4 : 3u * 160 * 4),
            "only the three changed rows were copied (%llu bytes)", (unsigned long long)(comp.upload_bytes - before));
   }

   /* ---- decorations the window manager does not make yet (step 1 of docs/gui/compositor-visual-plan.md): shapes and premultiplied pixels on top */
   {
      /* a "Luna" window frame: glossy gradient (a hard step at 45%), a border, a soft shadow below and to the right */
      struct cr_shape luna = { .radius = 10, .border = 2, .split = 0.45f, .shadow_blur = 12, .c = { 0xff3a80f3u, 0xff2a5fd8u, 0xff0b47c8u, 0xff2766e0u },
                               .border_color = 0xff0a246au, .shadow_color = 0x99000000u, .shadow_dx = 5, .shadow_dy = 8 };
      /* translucent glass, partly off the left edge, a horizontal gradient */
      struct cr_shape glass = { .radius = 16, .split = 1, .c = { 0x8840a0ffu, 0x55ffffffu, 0, 0 }, .horizontal = 1 };
      /* a ring: a transparent fill, a thick border, a radius past half the box (clamped), a hard shadow up and to the left */
      struct cr_shape ring = { .radius = 40, .border = 4, .split = 1, .c = { 0, 0, 0, 0 }, .border_color = 0xffffd700u, .shadow_color = 0x80ff0000u, .shadow_dx = -3, .shadow_dy = 3 };
      /* a "9x" title bar: square, navy to light blue left to right */
      struct cr_shape title = { .split = 1, .c = { 0xff000080u, 0xff1084d0u, 0, 0 }, .horizontal = 1 };
      /* off the bottom-right corner, its shadow too; a 3-stop gradient (c1 == c2) */
      struct cr_shape corner = { .radius = 6, .border = 1, .split = 0.5f, .shadow_blur = 6, .c = { 0xffff0000u, 0xff00ff00u, 0xff00ff00u, 0xff0000ffu },
                                 .border_color = 0xff000000u, .shadow_color = 0xff000000u, .shadow_dx = 4, .shadow_dy = 4 };
      /* an icon: premultiplied ARGB, a disc whose alpha falls off to the edge (what `img` decodes) */
      static uint32_t icon[48 * 48];
      for (int y = 0; y < 48; y++)
         for (int x = 0; x < 48; x++) {
            float dx = (float)x - 23.5f, dy = (float)y - 23.5f, dd = sqrtf(dx * dx + dy * dy);
            uint32_t a = dd >= 24 ? 0 : (uint32_t)(255.0f * (1.0f - dd / 24.0f));
            uint32_t r = (uint32_t)(x * 5) * a / 255, gg = (uint32_t)(y * 5) * a / 255, bl = 200 * a / 255;
            icon[y * 48 + x] = a << 24 | r << 16 | gg << 8 | bl;
         }
      struct cr_op deco[] = {
         { .kind = CR_SHAPE, .x = 330, .y = 70, .w = 220, .h = 150, .shape = luna },
         { .kind = CR_SHAPE, .x = -20, .y = 300, .w = 400, .h = 48, .shape = glass },
         { .kind = CR_SHAPE, .x = 580, .y = 20, .w = 50, .h = 50, .shape = ring },
         { .kind = CR_SHAPE, .x = 60, .y = 20, .w = 240, .h = 18, .shape = title },
         { .kind = CR_SHAPE, .x = 600, .y = 320, .w = 80, .h = 80, .shape = corner },
         { .kind = CR_CPU, .key = 1ull << 62, .version = 1, .px = icon, .npx = 48 * 48, .src_w = 48, .alpha = CR_PREMUL, .x = 100, .y = 200, .w = 48, .h = 48 },
         { .kind = CR_CPU, .key = 1ull << 62, .version = 1, .px = icon, .npx = 48 * 48, .src_w = 48, .alpha = CR_PREMUL, .x = 0, .y = 150, .w = 38, .h = 48, .sx = 10 },
      };
      /* the reference itself, on pixels whose answer is known, so a mistake shared by comp.frag and shape_pixel cannot pass */
      float px[4];
      shape_pixel(&deco[0], 330, 70, px);
      CHECK(px[3] < 0.05f, "the rounded corner is cut off (alpha %.3f)", px[3]);
      shape_pixel(&deco[0], 440, 71, px);
      CHECK(fabsf(px[2] - 0x6a / 255.0f) < 0.01f && px[3] > 0.99f, "the top border is the border colour (b %.3f a %.3f)", px[2], px[3]);
      shape_pixel(&deco[0], 440, 74, px);
      CHECK(fabsf(px[0] - 0x3a / 255.0f) < 0.03f && fabsf(px[2] - 0xf3 / 255.0f) < 0.03f, "just under the border the gradient starts at c0 (r %.3f b %.3f)", px[0], px[2]);
      shape_pixel(&deco[0], 440, 135, px);   /* the split is at y 70 + 0.45 * 150 = 137.5 */
      float after[4];
      shape_pixel(&deco[0], 440, 140, after);
      CHECK(fabsf(px[0] - 0x2a / 255.0f) < 0.03f && fabsf(after[0] - 0x0b / 255.0f) < 0.03f, "a hard step at the split: c1 just above, c2 just below (r %.3f -> %.3f)", px[0], after[0]);
      shape_pixel(&deco[0], 440, 224, px);
      CHECK(px[3] > 0.3f && px[3] < 0.55f && px[0] == 0.0f, "under the box: the soft, black shadow (a %.3f)", px[3]);
      shape_pixel(&deco[0], 600, 300, px);
      CHECK(px[3] == 0.0f, "far from it nothing (a %.3f)", px[3]);
      shape_pixel(&deco[2], 605, 45, px);
      CHECK(px[3] == 0.0f, "the ring's middle is transparent and its shadow is not seen through it (a %.3f)", px[3]);
      shape_pixel(&deco[2], 605, 21, px);
      CHECK(px[3] > 0.99f && fabsf(px[1] - 0xd7 / 255.0f) < 0.01f, "the ring's border (g %.3f)", px[1]);
      shape_pixel(&deco[3], 61, 25, px);
      float px2[4];
      shape_pixel(&deco[3], 298, 25, px2);
      CHECK(px[2] < 0.55f && px2[2] > 0.8f && px[3] == 1.0f, "the title bar runs navy to light blue left to right (b %.3f -> %.3f)", px[2], px2[2]);
      extra_ops = deco;
      n_extra = sizeof(deco) / sizeof(deco[0]);
      tol = 2;
      failures += frame(&v, &comp, dir, "5c-shapes") != 0;
      extra_ops = NULL;
      n_extra = 0;
      tol = 0;
   }

   /* ---- the window manager's own looks (step 2): its shapes go through gui_capi and the renderer like any operation */
   {
      struct gui_draw_op d;
      CHECK(gui_set_theme(g, "luna") == 0, "the Luna look");
      /* at a window's rounded corner three layers stack (the desktop's gradient, the shadow, the frame's anti-aliased edge), each rounded to
       * 8 bits where it is blended: lavapipe lands within 2 of the reference on a handful of those pixels */
      tol = 3;
      failures += frame(&v, &comp, dir, "5d-luna") != 0;
      int nshapes = 0;
      for (size_t i = 0; i < gui_draw_count(g); i++) if (gui_draw_get(g, i, &d) == 0 && d.kind == GUI_DRAW_SHAPE) nshapes++;
      /* the desktop, then per window a frame, a bar and its buttons (the pool window: close only; the GPU one: close only) */
      CHECK(nshapes >= 1 + 2 * 3, "Luna drew the desktop and both windows' frames, bars and buttons (%d shapes)", nshapes);
      CHECK(gui_set_theme(g, "9x") == 0, "the 9x look");
      failures += frame(&v, &comp, dir, "5e-9x") != 0;
      CHECK(gui_set_theme(g, "flat") == 0, "back to flat: the frames below compare exactly");
      tol = 0;
   }

   /* ---- the pool window grows (a bigger buffer than the one its upload buffer was made for), and the pointer sits on the right/bottom edges */
   {
      int bw = 220, bh = 130;
      int fd3 = memfd_create("pool-big", 0);
      ftruncate(fd3, bw * bh * 4);
      uint32_t *big = mmap(NULL, (size_t)bw * bh * 4, PROT_READ | PROT_WRITE, MAP_SHARED, fd3, 0);
      for (int y = 0; y < bh; y++) for (int x = 0; x < bw; x++) big[y * bw + x] = ((x / 5 + y / 7) & 1) ? 0x0030a050u : 0x00a05030u;
      memset(&o, 0, sizeof(o));
      guiw_create_pool(&o, 6, dup(fd3), (uint32_t)(bw * bh * 4));
      guiw_create_buffer(&o, 6, 7, 0, bw, bh, bw * 4, GUIW_FORMAT_XRGB8888);
      guiw_attach(&o, 4, 7);
      guiw_commit(&o, 4);
      send_out(b.id, &o);
      gui_pointer_motion(g, 2 * W, 2 * H);          /* clamps to the bottom-right corner */
      failures += frame(&v, &comp, dir, "8-grown-and-corner") != 0;
   }

   /* ---- the client leaves: its buffers are dropped and the screen shows the rest */
   gui_remove_client(g, a.id);
   failures += frame(&v, &comp, dir, "6-a-gone") != 0;
   failures += frame(&v, &comp, dir, "7-idle") != 0;
   int live = 0;
   for (int i = 0; i < COMP_MAX_GPU; i++) if (comp.gpu[i].key) live++;
   CHECK(live == 0 && comp.drops == 2, "both GPU buffers were dropped and freed (%d live, %u drops)", live, comp.drops);

   comp_destroy(&comp);
   printf("host_comp: %u frames, up to %u draws each, %u imports, %u uploads\n", comp.frames, comp.draws_max, comp.imports, comp.uploads);
   if (failures) { printf("HOST_COMP FAILED (%d)\n", failures); return 1; }
   printf("HOST_COMP DONE\n");
   return 0;
}
