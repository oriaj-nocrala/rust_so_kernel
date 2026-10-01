/*
 * host_comp: the GPU compositor's renderer (comp_render.h) driven on the host's own Vulkan with fake clients, against a CPU reference
 * (G5 layer 4, slice 3). `probes/nvk/host-comp.sh [dump-dir]` builds and runs it. The window manager is the real one (gui_capi), the clients are
 * in-process: a "GPU" client whose buffer is a memfd it writes pixels into (the harness's stand-in for a client's VRAM buffer), and a
 * pool client (shm) for the CPU path. Each frame is rendered into an offscreen image, read back, and compared pixel for pixel with a
 * rasterisation of the same draw list on the CPU (titles are not drawn by either). Exit 1 on the first difference.
 */
#define _GNU_SOURCE
#include <vulkan/vulkan.h>

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
   struct cr_op *ops = calloc(nops + 1, sizeof(*ops));
   size_t no = 0;
   for (size_t i = 0; i < nops; i++) {
      struct gui_draw_op d;
      gui_draw_get(g, i, &d);
      struct cr_op *o = &ops[no];
      switch (d.kind) {
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
         *o = (struct cr_op){ .kind = CR_CPU, .key = 1ull << 63, .version = 1, .px = cursor_px, .npx = GUI_CURSOR_W * GUI_CURSOR_H, .src_w = GUI_CURSOR_W, .keyed = 1,
                              .x = d.x, .y = d.y, .w = GUI_CURSOR_W, .h = GUI_CURSOR_H };
         no++;
         break;
      default: break;
      }
   }
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
   int bad = 0, bx = -1, by = -1;
   for (int y = 0; y < H; y++) {
      for (int x = 0; x < W; x++) {
         uint32_t got = v->rb_map[y * W + x] & 0x00ffffffu, w = want[y * W + x] & 0x00ffffffu;
         if (got != w && !bad++) { bx = x; by = y; }
      }
   }
   dump_ppm(dir, name, v->rb_map);
   printf("frame %s: %u draws, %u uploads, %d pixels differ", name, c->draws, c->uploads, bad);
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
