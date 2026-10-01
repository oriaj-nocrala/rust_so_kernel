// gui_fake_comp: a stand-in compositor for the clients of GPU buffers (G5 layer 4, slice 2; docs/gpu/g5-graphics-stack-plan.md).
//
//   gui_fake_comp [command [args...]]     (default: /mnt/bin/vk_window)
//
// Listens on /tmp/gui-test, starts the command with GUI_DISPLAY pointing at it and serves its one connection like the compositor would
// for GPU buffers: it answers create_surface with `configure`, imports every create_gpu_buffer descriptor into its own /dev/nvgpu session
// (BO_IMPORT: the proof that what the client exported is a buffer another process can take), remembers the surface's current buffer at
// each commit and releases the one it replaced after FAKE_COMP_DELAY_MS (default 5; 0 = at once), so a client that has run out of images has
// to wait for a release, as it would for the real one. It reads no pixels (a buffer of VRAM is not mappable by the CPU, and the software
// device draws nothing): it checks the protocol and the bookkeeping, and the real compositor (vk_comp) checks the picture.
//
// Fails (exit 1) if the client's exit status is not 0, a request is malformed or names something that does not exist, a buffer is
// smaller than its stride and height say, an import fails, a commit has no buffer, or the client ended with buffers it never destroyed.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include "constanos_gui_wire.h"
#include "nvgpu.h"

#define SOCK_PATH "/tmp/gui-test"
#define MAX_BUFS 64
#define MAX_PENDING 64

static int failures;
#define FAIL(...) do { failures++; printf("FAKE COMP FAIL: "); printf(__VA_ARGS__); printf("\n"); } while (0)

struct buf {
   uint32_t id;       // 0 = free
   uint32_t handle;   // the BO in our session
   uint32_t w, h, stride;
   uint64_t size;
   int held;          // committed and not released yet: the client must not draw into it or commit it
};

static struct buf bufs[MAX_BUFS];
static int nvg = -1;
static int cl = -1;                 // the client's connection
static uint8_t rx[8192];
static size_t rxlen;
static int fdq[16];
static int nfdq;

static uint32_t surface_id, current, pending;
static uint32_t frame_ids[8];
static int nframes;
static unsigned frames_asked, frames_answered;
static unsigned imported, destroyed, commits, releases, same_buffer_commits;
static unsigned long long bytes_imported;
static uint32_t last_w, last_h;
static unsigned sizes_seen;
static uint32_t size_list[8][2];
static char title[128];

struct due { uint64_t at_ms; uint32_t id; };
static struct due dues[MAX_PENDING];
static int ndue;
static int delay_ms = 5;

static uint64_t now_ms(void) {
   struct timespec t;
   clock_gettime(CLOCK_MONOTONIC, &t);
   return (uint64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}

static struct buf *find_buf(uint32_t id) {
   for (int i = 0; i < MAX_BUFS; i++)
      if (bufs[i].id == id && id) return &bufs[i];
   return NULL;
}

static void send_msg(struct guiw_out *o) {
   if (o->overflow || cl < 0) return;
   if (send(cl, o->bytes, o->len, MSG_NOSIGNAL) < 0) { /* the client is gone: the read loop notices */ }
   memset(o, 0, sizeof(*o));
}

static void ev_configure(uint32_t surface, int w, int h) {
   struct guiw_out o;
   memset(&o, 0, sizeof(o));
   guiw_begin(&o, surface, GUIW_EV_CONFIGURE); guiw_put(&o, (uint32_t)w); guiw_put(&o, (uint32_t)h); guiw_end(&o);
   send_msg(&o);
}

static void ev_release(uint32_t buffer) {
   struct guiw_out o;
   memset(&o, 0, sizeof(o));
   guiw_begin(&o, buffer, GUIW_EV_RELEASE); guiw_end(&o);
   send_msg(&o);
   releases++;
}

static void ev_delete_id(uint32_t id) {
   struct guiw_out o;
   memset(&o, 0, sizeof(o));
   guiw_begin(&o, GUIW_COMPOSITOR, GUIW_EV_DELETE_ID); guiw_put(&o, id); guiw_end(&o);
   send_msg(&o);
}

static void release_later(uint32_t id) {
   if (delay_ms <= 0) { struct buf *b = find_buf(id); if (b) b->held = 0; ev_release(id); return; }
   if (ndue < MAX_PENDING) dues[ndue++] = (struct due){ now_ms() + (uint64_t)delay_ms, id };
}

static void flush_dues(void) {
   uint64_t t = now_ms();
   for (int i = 0; i < ndue;) {
      if (dues[i].at_ms <= t) {
         // the client may have destroyed the buffer in the meantime: then there is nobody to tell
         struct buf *b = find_buf(dues[i].id);
         if (b) { b->held = 0; ev_release(dues[i].id); }
         dues[i] = dues[--ndue];
      } else {
         i++;
      }
   }
}

static void note_size(uint32_t w, uint32_t h) {
   last_w = w; last_h = h;
   for (unsigned i = 0; i < sizes_seen; i++)
      if (size_list[i][0] == w && size_list[i][1] == h) return;
   if (sizes_seen < 8) { size_list[sizes_seen][0] = w; size_list[sizes_seen][1] = h; sizes_seen++; }
}

static void create_gpu_buffer(const struct guiw_msg *m) {
   if (guiw_nargs(m) != 6) { FAIL("create_gpu_buffer with %d arguments", guiw_nargs(m)); return; }
   uint32_t id = guiw_arg(m, 0), size = guiw_arg(m, 1), w = guiw_arg(m, 2), h = guiw_arg(m, 3), stride = guiw_arg(m, 4), format = guiw_arg(m, 5);
   if (nfdq == 0) { FAIL("create_gpu_buffer %u came without a descriptor", id); return; }
   int fd = fdq[0];
   memmove(fdq, fdq + 1, sizeof(int) * (size_t)--nfdq);
   if (id < 3 || find_buf(id)) FAIL("buffer id %u is reserved or in use", id);
   if (format != GUIW_FORMAT_XRGB8888) FAIL("buffer %u has format %u", id, format);
   if (w == 0 || h == 0 || stride < w * 4 || stride % 4 || (uint64_t)stride * (h - 1) + (uint64_t)w * 4 > size) FAIL("buffer %u: %ux%u stride %u in %u bytes does not add up", id, w, h, stride, size);
   struct nvg_bo_import im = { .fd = fd };
   if (ioctl(nvg, NVG_IOC_BO_IMPORT, &im) != 0) {
      FAIL("BO_IMPORT of buffer %u failed (errno %d)", id, errno);
      close(fd);
      return;
   }
   close(fd);
   if (im.size_out < size) FAIL("buffer %u: the BO is %llu bytes, the client said %u", id, (unsigned long long)im.size_out, size);
   for (int i = 0; i < MAX_BUFS; i++) {
      if (!bufs[i].id) {
         bufs[i] = (struct buf){ .id = id, .handle = im.handle, .w = w, .h = h, .stride = stride, .size = im.size_out };
         imported++;
         bytes_imported += im.size_out;
         return;
      }
   }
   FAIL("more than %d buffers at once", MAX_BUFS);
}

static void handle(const struct guiw_msg *m) {
   if (m->object == GUIW_COMPOSITOR) {
      if (m->opcode == 1 && guiw_nargs(m) == 1) {
         surface_id = guiw_arg(m, 0);
         ev_configure(surface_id, 320, 240);
      } else if (m->opcode == 3) {
         create_gpu_buffer(m);
      } else {
         FAIL("compositor request %u is not one this stand-in takes", m->opcode);
      }
      return;
   }
   if (surface_id && m->object == surface_id) {
      switch (m->opcode) {
      case 0: // attach
         pending = guiw_arg(m, 0);
         if (pending && !find_buf(pending)) FAIL("attach of buffer %u, which does not exist", pending);
         break;
      case 1: break; // damage
      case 2: // frame(callback)
         if (nframes < 8) frame_ids[nframes++] = guiw_arg(m, 0);
         frames_asked++;
         break;
      case 3: { // commit
         struct buf *b = find_buf(pending);
         if (!b) { FAIL("commit with no buffer attached"); break; }
         commits++;
         /* the frame callbacks asked since the last commit are answered when it is "shown": here, at once (a real compositor does after drawing) */
         for (int i = 0; i < nframes; i++) {
            struct guiw_out d;
            memset(&d, 0, sizeof(d));
            guiw_begin(&d, frame_ids[i], GUIW_EV_DONE); guiw_put(&d, 0); guiw_end(&d);
            send_msg(&d);
            ev_delete_id(frame_ids[i]);
            frames_answered++;
         }
         nframes = 0;
         note_size(b->w, b->h);
         if (current == pending) {
            same_buffer_commits++;
         } else {
            // a buffer the compositor still holds (shown, or replaced and not released yet) is not the client's to commit again
            if (b->held) FAIL("buffer %u was committed again before its release (commit %u)", b->id, commits);
            if (current && find_buf(current)) release_later(current);
            current = pending;
            b->held = 1;
         }
         break;
      }
      case 4: { // set_title
         uint32_t n = guiw_arg(m, 0);
         if (n > 0 && n < sizeof(title) && (uint32_t)(m->size - 12) >= n) { memcpy(title, m->args + 4, n); title[n - 1] = 0; }
         break;
      }
      default: break;
      }
      return;
   }
   struct buf *b = find_buf(m->object);
   if (b && m->opcode == 0) { // destroy
      struct nvg_bo_free f = { .handle = b->handle };
      ioctl(nvg, NVG_IOC_BO_FREE, &f);
      if (current == b->id) current = 0;
      if (pending == b->id) pending = 0;
      b->id = 0;
      destroyed++;
      ev_delete_id(m->object);
      return;
   }
   FAIL("a request (object %u, opcode %u) for something that does not exist", m->object, m->opcode);
}

// Reads what the client sent (with its descriptors) and handles every whole message. Returns 0 at EOF.
static int serve_readable(void) {
   char ctl[CMSG_SPACE(sizeof(int) * 8)];
   struct iovec iov = { rx + rxlen, sizeof(rx) - rxlen };
   struct msghdr mh;
   memset(&mh, 0, sizeof(mh));
   mh.msg_iov = &iov;
   mh.msg_iovlen = 1;
   mh.msg_control = ctl;
   mh.msg_controllen = sizeof(ctl);
   long n = recvmsg(cl, &mh, 0);
   if (n <= 0) return 0;
   for (struct cmsghdr *c = CMSG_FIRSTHDR(&mh); c; c = CMSG_NXTHDR(&mh, c)) {
      if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS) {
         int cnt = (int)((c->cmsg_len - CMSG_LEN(0)) / sizeof(int));
         for (int i = 0; i < cnt; i++) {
            int fd;
            memcpy(&fd, CMSG_DATA(c) + i * sizeof(int), sizeof(int));
            if (nfdq < 16) fdq[nfdq++] = fd; else close(fd);
         }
      }
   }
   rxlen += (size_t)n;
   size_t off = 0;
   for (;;) {
      struct guiw_msg m;
      int k = guiw_next(rx + off, rxlen - off, &m);
      if (k < 0) { FAIL("malformed message header"); return 0; }
      if (k == 0) break;
      off += (size_t)k;
      handle(&m);
   }
   memmove(rx, rx + off, rxlen - off);
   rxlen -= off;
   return 1;
}

int main(int argc, char **argv) {
   setvbuf(stdout, NULL, _IONBF, 0);
   signal(SIGPIPE, SIG_IGN);
   const char *e;
   if ((e = getenv("FAKE_COMP_DELAY_MS"))) delay_ms = atoi(e);
   nvg = open("/dev/nvgpu", O_RDWR);
   if (nvg < 0) { printf("FAKE COMP FAIL: cannot open /dev/nvgpu (errno %d)\n", errno); return 1; }

   int lfd = socket(AF_UNIX, SOCK_STREAM, 0);
   struct sockaddr_un a;
   memset(&a, 0, sizeof(a));
   a.sun_family = AF_UNIX;
   strcpy(a.sun_path, SOCK_PATH);
   unlink(SOCK_PATH);
   if (lfd < 0 || bind(lfd, (struct sockaddr *)&a, sizeof(a)) < 0 || listen(lfd, 1) < 0) { printf("FAKE COMP FAIL: cannot listen on %s (errno %d)\n", SOCK_PATH, errno); return 1; }

   const char *default_cmd[] = { "/mnt/bin/vk_window", NULL };
   char **cmd = argc > 1 ? argv + 1 : (char **)default_cmd;
   pid_t kid = fork();
   if (kid == 0) {
      setenv("GUI_DISPLAY", SOCK_PATH, 1);
      execv(cmd[0], cmd);
      printf("FAKE COMP FAIL: cannot run %s (errno %d)\n", cmd[0], errno);
      _exit(127);
   }
   printf("FAKE COMP: listening on %s, running %s (releases %d ms after the commit that replaces)\n", SOCK_PATH, cmd[0], delay_ms);

   struct pollfd pf = { .fd = lfd, .events = POLLIN };
   int waited = 0;
   while (poll(&pf, 1, 100) <= 0)
      if (++waited > 600) { FAIL("the client never connected"); kill(kid, SIGKILL); goto reap; }
   cl = accept(lfd, NULL, NULL);
   if (cl < 0) { FAIL("accept failed"); goto reap; }

   for (;;) {
      flush_dues();
      pf = (struct pollfd){ .fd = cl, .events = POLLIN };
      int r = poll(&pf, 1, ndue ? 2 : 200);
      if (r > 0 && (pf.revents & (POLLIN | POLLHUP))) {
         if (!serve_readable()) break;
      }
   }

reap:;
   int st = 0;
   waitpid(kid, &st, 0);
   int code = WIFEXITED(st) ? WEXITSTATUS(st) : -1;
   unsigned left = 0;
   for (int i = 0; i < MAX_BUFS; i++) if (bufs[i].id) left++;
   printf("FAKE COMP: \"%s\": %u buffers imported (%llu KiB), %u destroyed, %u still held at the end, %u commits (%u of the same buffer), %u releases sent, %u size(s):",
          title, imported, bytes_imported >> 10, destroyed, left, commits, same_buffer_commits, releases, sizes_seen);
   for (unsigned i = 0; i < sizes_seen; i++) printf(" %ux%u", size_list[i][0], size_list[i][1]);
   printf("\n");
   if (code != 0) FAIL("the client exited with %d", code);
   if (frames_asked != commits || frames_answered != frames_asked) FAIL("%u frame callbacks asked, %u answered, %u commits: each commit must ask for one", frames_asked, frames_answered, commits);
   if (imported == 0 || commits == 0) FAIL("nothing was imported or committed");
   // what is still held was never destroyed by the client: the swapchain's last images are destroyed with it, so any left over is a leak
   if (left) FAIL("%u buffers were never destroyed", left);
   unlink(SOCK_PATH);
   if (failures) { printf("FAKE COMP FAILED (%d)\n", failures); return 1; }
   printf("FAKE COMP DONE\n");
   return 0;
}
