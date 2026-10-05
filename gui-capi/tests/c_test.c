// Drives the window manager through gui_capi.h the way vk_comp.c does: bytes from clients in, events / GPU ops / draw operations out.
// Built with the host's cc against the static library by tests/c_api.rs; prints "ok ..." lines and exits 1 on the first failure.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#include "constanos_gui_wire.h"
#include "gui_capi.h"

#define CHECK(c) do { if (c) printf("ok   %s\n", #c); else { printf("FAIL %s (line %d)\n", #c, __LINE__); exit(1); } } while (0)

static void send_out(gui_comp *c, uint32_t client, struct guiw_out *o) {
   CHECK(!o->overflow);
   gui_client_data(c, client, o->bytes, o->len, o->fds, (size_t)o->nfds);
   memset(o, 0, sizeof(*o));
}

// Every event queued for `client`, as (object, opcode); returns how many.
struct ev { uint32_t object; uint16_t opcode; uint32_t a0, a1; };
static int events_for(gui_comp *c, uint32_t client, struct ev *out, int max) {
   int n = 0;
   uint8_t buf[4096];
   uint32_t who;
   size_t len;
   while ((len = gui_pop_event(c, &who, buf, sizeof(buf))) > 0) {
      struct guiw_msg m = {0};
      CHECK(guiw_next(buf, len, &m) == (int)len);
      if (who == client && n < max) {
         out[n].object = m.object; out[n].opcode = m.opcode;
         out[n].a0 = guiw_nargs(&m) > 0 ? guiw_arg(&m, 0) : 0;
         out[n].a1 = guiw_nargs(&m) > 1 ? guiw_arg(&m, 1) : 0;
         n++;
      }
   }
   return n;
}

static int has_event(const struct ev *e, int n, uint32_t object, uint16_t opcode) {
   for (int i = 0; i < n; i++) if (e[i].object == object && e[i].opcode == opcode) return 1;
   return 0;
}

static int find_op(gui_comp *c, uint32_t kind, struct gui_draw_op *out) {
   for (size_t i = 0; i < gui_draw_count(c); i++) {
      struct gui_draw_op op;
      if (gui_draw_get(c, i, &op) == 0 && op.kind == kind) { *out = op; return 1; }
   }
   return 0;
}

int main(void) {
   gui_comp *c = gui_new(320, 240);
   CHECK(c != NULL);

   // ---- a client that does not have GPU buffers enabled is disconnected
   uint32_t a = gui_add_client(c);
   struct guiw_out o;
   memset(&o, 0, sizeof(o));
   guiw_create_gpu_buffer(&o, 3, 90, 64 * 32 * 4, 64, 32, 256, GUIW_FORMAT_XRGB8888);
   send_out(c, a, &o);
   uint32_t gone;
   CHECK(gui_pop_disconnect(c, &gone) == 1 && gone == a);
   int32_t fd;
   CHECK(gui_pop_fd_to_close(c, &fd) == 1 && fd == 90);
   struct gui_gpu_op gop;
   CHECK(gui_pop_gpu_op(c, &gop) == 0);

   // ---- with them enabled: import, commit, draw, release
   gui_enable_gpu_buffers(c);
   uint32_t g = gui_add_client(c);
   guiw_create_surface(&o, 2);
   guiw_create_gpu_buffer(&o, 3, 77, 64 * 32 * 4, 64, 32, 256, GUIW_FORMAT_XRGB8888);
   guiw_create_gpu_buffer(&o, 5, 78, 64 * 32 * 4, 64, 32, 256, GUIW_FORMAT_XRGB8888);
   guiw_attach(&o, 2, 3);
   guiw_commit(&o, 2);
   send_out(c, g, &o);
   CHECK(gui_pop_gpu_op(c, &gop) == 1 && gop.kind == GUI_GPU_IMPORT && gop.fd == 77 && gop.width == 64 && gop.height == 32 && gop.stride == 256 && gop.size == 8192);
   uint64_t first = gop.handle;
   CHECK(gui_pop_gpu_op(c, &gop) == 1 && gop.kind == GUI_GPU_IMPORT && gop.fd == 78 && gop.handle != first);
   uint64_t second = gop.handle;
   CHECK(gui_pop_gpu_op(c, &gop) == 0);
   struct ev ev[16];
   int n = events_for(c, g, ev, 16);
   CHECK(has_event(ev, n, 2, GUIW_EV_CONFIGURE));
   CHECK(!has_event(ev, n, 3, GUIW_EV_RELEASE));          // a GPU buffer is not released at commit
   CHECK(gui_has_damage(c));
   uint64_t e1 = gui_draw_list(c);
   CHECK(e1 == 1 && !gui_has_damage(c));
   struct gui_draw_op op;
   CHECK(find_op(c, GUI_DRAW_GPU, &op) && op.handle == first && op.w == 64 && op.h == 32 && op.sx == 0 && op.sy == 0);
   CHECK(find_op(c, GUI_DRAW_FILL, &op));                  // the background, first of the list
   CHECK(find_op(c, GUI_DRAW_TITLE, &op) && op.focused == 1 && op.w > 0 && op.title_fg == 0x00F0F0F0u && op.title_shadow == 0);
   size_t ti = 0;
   for (size_t i = 0; i < gui_draw_count(c); i++) { struct gui_draw_op t; gui_draw_get(c, i, &t); if (t.kind == GUI_DRAW_TITLE) ti = i; }
   guiw_set_title(&o, 2, "ventana");
   send_out(c, g, &o);
   uint64_t e_title = gui_draw_list(c);   // frame 2 read buffer 3 as well
   CHECK(e_title == 2);
   for (size_t i = 0; i < gui_draw_count(c); i++) { struct gui_draw_op t; gui_draw_get(c, i, &t); if (t.kind == GUI_DRAW_TITLE) ti = i; }
   CHECK(gui_title(c, ti) != NULL && strcmp(gui_title(c, ti), "ventana") == 0);

   // the client moves to its second buffer: the first is released when frame e2 (which read it before) is done, not before
   guiw_attach(&o, 2, 5);
   guiw_commit(&o, 2);
   send_out(c, g, &o);
   n = events_for(c, g, ev, 16);
   CHECK(!has_event(ev, n, 3, GUIW_EV_RELEASE));
   uint64_t e2 = gui_draw_list(c);
   CHECK(find_op(c, GUI_DRAW_GPU, &op) && op.handle == second);
   gui_gpu_frame_done(c, e1);
   n = events_for(c, g, ev, 16);
   CHECK(!has_event(ev, n, 3, GUIW_EV_RELEASE));           // frame 2 read it too
   gui_gpu_frame_done(c, e_title);
   n = events_for(c, g, ev, 16);
   CHECK(has_event(ev, n, 3, GUIW_EV_RELEASE));
   gui_gpu_frame_done(c, e2);
   n = events_for(c, g, ev, 16);
   CHECK(!has_event(ev, n, 5, GUIW_EV_RELEASE));           // the one on screen stays the compositor's

   // a window dragged past the left and top edges is drawn from the inside of its buffer (the source offset is not 0)
   {
      struct gui_draw_op w;
      CHECK(find_op(c, GUI_DRAW_GPU, &w));
      // grab the title bar and carry the window up and to the left, off the screen
      struct gui_draw_op bar;
      CHECK(find_op(c, GUI_DRAW_TITLE, &bar));
      int px0 = bar.x + 5, py0 = bar.y + 3;
      gui_pointer_motion(c, px0 - 160, py0 - 120);   // the pointer starts at the centre (160, 120)
      gui_pointer_button(c, 0x110, 1);
      gui_pointer_motion(c, -60, -70);
      gui_pointer_button(c, 0x110, 0);
      gui_draw_list(c);
      struct gui_draw_op cut;
      CHECK(find_op(c, GUI_DRAW_GPU, &cut) && cut.handle == second);
      CHECK(cut.sx > 0 && cut.x == 0);                // cut by the left edge: starts inside the buffer, drawn at the edge
      CHECK(cut.w == 64 - cut.sx);
      CHECK(cut.sy >= 0 && cut.y >= 0);
   }

   // destroying a buffer and the client leaving drop the handles
   gui_remove_client(c, g);
   int drops = 0;
   while (gui_pop_gpu_op(c, &gop)) { CHECK(gop.kind == GUI_GPU_DROP); drops++; }
   CHECK(drops == 2);

   // ---- a pool window: a real memfd, mapped by the library; its pixels come back through gui_cpu_content
   uint32_t p = gui_add_client(c);
   int mfd = memfd_create("pool", 0);
   CHECK(mfd >= 0 && ftruncate(mfd, 40 * 20 * 4) == 0);
   uint32_t *px = mmap(NULL, 40 * 20 * 4, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
   CHECK(px != MAP_FAILED);
   for (int i = 0; i < 40 * 20; i++) px[i] = 0x00112233u + (uint32_t)i;
   guiw_create_pool(&o, 2, mfd, 40 * 20 * 4);
   guiw_create_buffer(&o, 2, 3, 0, 40, 20, 160, GUIW_FORMAT_XRGB8888);
   guiw_create_surface(&o, 4);
   guiw_attach(&o, 4, 3);
   guiw_commit(&o, 4);
   send_out(c, p, &o);
   n = events_for(c, p, ev, 16);
   CHECK(has_event(ev, n, 3, GUIW_EV_RELEASE));            // a pool buffer is copied and released at commit, as always
   CHECK(gui_pop_fd_to_close(c, &fd) == 1 && fd == mfd);   // the library mapped the pool and gives the descriptor back
   gui_draw_list(c);
   CHECK(find_op(c, GUI_DRAW_CPU, &op) && op.client == p && op.surface == 4 && op.src_w == 40 && op.src_h == 20 && op.w == 40 && op.h == 20);
   size_t len = 0;
   const uint32_t *got = gui_cpu_content(c, p, 4, &len);
   CHECK(got != NULL && len == 40 * 20 && got[0] == 0x00112233u && got[40 * 20 - 1] == 0x00112233u + 799u);
   CHECK(gui_cpu_content(c, p, 99, &len) == NULL);
   CHECK(find_op(c, GUI_DRAW_CURSOR, &op));
   CHECK(gui_cursor_bitmap(0) != NULL && gui_cursor_bitmap(0)[0] == 'X' && gui_cursor_bitmap(GUI_CURSOR_H) == NULL);
   close(mfd);

   // ---- a pool the client says is bigger than the file behind it is refused, not mapped
   {
      uint32_t q = gui_add_client(c);
      int small = memfd_create("small", 0);
      CHECK(small >= 0 && ftruncate(small, 100) == 0);
      guiw_create_pool(&o, 2, small, 4096);
      send_out(c, q, &o);
      uint32_t who;
      CHECK(gui_pop_disconnect(c, &who) == 1 && who == q);
      int32_t cl;
      CHECK(gui_pop_fd_to_close(c, &cl) == 1 && cl == small);
      close(small);
   }

   // ---- input reaches the focused window
   gui_set_time(c, 1000);
   gui_key(c, 30, 1);
   n = events_for(c, p, ev, 16);
   CHECK(has_event(ev, n, 4, GUIW_EV_KEY) && ev[n - 1].a0 == 30 && ev[n - 1].a1 == 1);
   gui_pointer_motion(c, 5, 5);
   CHECK(!gui_quit_requested(c));

   // ---- looks: a theme of shapes puts the desktop's gradient first and the window's frame after it; an unknown name changes nothing
   CHECK(gui_set_theme(c, "aqua") == -1);
   CHECK(gui_set_theme(c, "luna") == 0);
   gui_draw_list(c);
   struct gui_draw_op sh;
   CHECK(gui_draw_get(c, 0, &sh) == 0 && sh.kind == GUI_DRAW_SHAPE && sh.x == 0 && sh.y == 0 && sh.w == 320 && sh.h == 240 && sh.shape_split > 0.5f);
   CHECK(gui_draw_get(c, 1, &sh) == 0 && sh.kind == GUI_DRAW_SHAPE && sh.shape_radius == 8.0f && (sh.shape_shadow_color >> 24) != 0 && sh.shape_shadow_dy == 6);
   CHECK(find_op(c, GUI_DRAW_TITLE, &op) && op.title_fg == 0x00FFFFFFu && (op.title_shadow >> 24) != 0);
   CHECK(gui_set_theme(c, "flat") == 0);
   gui_draw_list(c);
   CHECK(gui_draw_get(c, 0, &sh) == 0 && sh.kind == GUI_DRAW_FILL);
   gui_free(c);
   printf("gui_capi: DONE\n");
   return 0;
}
