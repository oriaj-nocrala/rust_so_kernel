// gui_comp_test: the GPU compositor (vk_comp) with real clients in QEMU, on the software device (G5 layer 4, slice 3c). Two cases: vk_window (below)
// and snake3d in a window with F11 pressed twice by the compositor itself (COMP_F11_AT): fullscreen and back, each a resize the client answers by
// making its swapchain again (3 + 3 + 3 buffers imported and dropped, the two old swapchains destroyed by the client itself, two "resized to"
// lines, a clean exit).
//
// Starts vk_comp headless (no display, no input devices), waits for it to listen, runs vk_window against it (60 frames, a resize halfway:
// two swapchains of three GPU buffers each), then asks vk_comp to quit and checks what each said: the client's checks passed, and the compositor
// imported all six buffers, composed frames, dropped the six buffers when the client left, and exited cleanly. The software device draws
// nothing, so this proves the flow (sockets, descriptors, the opaque-fd import, release and drop), not the picture: the host harness
// (probes/nvk/host-comp.sh) checks the pixels of the renderer and the Ryzen job checks the real thing.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define SOCK "/tmp/gui-comp-test"

static int failures;
#define FAIL(...) do { failures++; printf("gui_comp_test FAIL: "); printf(__VA_ARGS__); printf("\n"); } while (0)

static long now_ms(void) {
   struct timespec t;
   clock_gettime(CLOCK_MONOTONIC, &t);
   return t.tv_sec * 1000 + t.tv_nsec / 1000000;
}

static char out[65536];
static size_t outlen;
static char cout_[16384];   /* what the client printed */
static size_t coutlen;

/* Reads whatever a program printed (its pipe is non-blocking). */
static void drain_into(int fd, char *buf, size_t cap, size_t *len) {
   for (;;) {
      long n = read(fd, buf + *len, cap - 1 - *len);
      if (n <= 0) break;
      *len += (size_t)n;
      buf[*len] = 0;
      if (*len >= cap - 1) { *len = 0; }   /* keep the tail only: the summary is at the end */
   }
}
static void drain(int fd) { drain_into(fd, out, sizeof(out), &outlen); }

struct kase {
   const char *name;
   const char *client;            /* the program, started with $GUI_DISPLAY set */
   const char *comp_env[4];       /* "K=V", NULL-terminated */
   const char *comp_args[3];      /* programs vk_comp starts itself */
   const char *client_env[6];
   unsigned want_buffers;         /* imported and dropped */
   unsigned min_frames;
   const char *client_must_print[4];
   unsigned want_resizes;         /* "resized to" lines in the client's output (0: none required) */
};

static const struct kase cases[] = {
   { "vk_window", "/mnt/bin/vk_window", { NULL }, { NULL }, { NULL }, 6, 40, { "VK WINDOW DONE", NULL }, 0 },
   /* cpumon (a CPU-drawn Rust client, in a pool buffer) beside a GPU client, and it does not exit on its own: the compositor ends it */
   { "vk_window + cpumon", "/mnt/bin/vk_window", { NULL }, { "cpumon", NULL }, { NULL }, 6, 40, { "VK WINDOW DONE", NULL }, 0 },
   { "snake3d window + F11", "/mnt/bin/snake3d",
     { "COMP_F11_AT=60,140", NULL }, { NULL },
     { "SNAKE3D_WINDOW=1", "SNAKE3D_AUTOPLAY=1", "SNAKE3D_SECONDS=8", NULL },
     9, 60, { "SNAKE3D DONE", "SNAKE3D window: 2 resizes", "6 buffers destroyed so far", NULL }, 2 },
};

static int run_case(const struct kase *k) {
   outlen = coutlen = 0;
   out[0] = cout_[0] = 0;
   const int before = failures;
   printf("gui_comp_test: --- %s\n", k->name);
   unlink(SOCK);
   int p[2], cp[2];
   if (pipe(p) != 0 || pipe(cp) != 0) { FAIL("pipe"); return 1; }
   pid_t comp = fork();
   if (comp == 0) {
      dup2(p[1], 1);
      dup2(p[1], 2);
      close(p[0]);
      setenv("COMP_HEADLESS", "1", 1);
      setenv("COMP_NO_INPUT", "1", 1);
      setenv("COMP_SOCKET", SOCK, 1);
      for (int i = 0; k->comp_env[i]; i++) putenv((char *)k->comp_env[i]);
      char *argv[4] = { "/mnt/bin/vk_comp", NULL, NULL, NULL };
      for (int i = 0; k->comp_args[i]; i++) argv[1 + i] = (char *)k->comp_args[i];
      execv(argv[0], argv);
      _exit(127);
   }
   close(p[1]);
   fcntl(p[0], F_SETFL, O_NONBLOCK);

   long t0 = now_ms();
   while (!strstr(out, "COMP listening") && now_ms() - t0 < 60000) {
      drain(p[0]);
      int st;
      if (waitpid(comp, &st, WNOHANG) == comp) { FAIL("vk_comp ended before it listened (status %d)\n%s", st, out); return 1; }
      usleep(20000);
   }
   if (!strstr(out, "COMP listening")) { FAIL("vk_comp never listened:\n%s", out); kill(comp, SIGKILL); return 1; }
   printf("gui_comp_test: vk_comp listens (%ld ms)\n", now_ms() - t0);
   if (k->comp_args[0]) {
      /* the compositor starts its own program on a thread: the client must not finish (and the compositor be told to quit) before that exec is done */
      t0 = now_ms();
      while (!strstr(out, "COMP started") && !strstr(out, "COMP cannot start") && now_ms() - t0 < 90000) { drain(p[0]); usleep(20000); }
      printf("gui_comp_test: the compositor's program %s (%ld ms)\n", strstr(out, "COMP started") ? "started" : "did not start", now_ms() - t0);
   }

   pid_t cl = fork();
   if (cl == 0) {
      dup2(cp[1], 1);
      dup2(cp[1], 2);
      close(cp[0]);
      setenv("GUI_DISPLAY", SOCK, 1);
      for (int i = 0; k->client_env[i]; i++) putenv((char *)k->client_env[i]);
      char *argv[] = { (char *)k->client, NULL };
      execv(argv[0], argv);
      _exit(127);
   }
   close(cp[1]);
   fcntl(cp[0], F_SETFL, O_NONBLOCK);
   int cst = 0;
   t0 = now_ms();
   for (;;) {
      drain(p[0]);
      drain_into(cp[0], cout_, sizeof(cout_), &coutlen);
      pid_t r = waitpid(cl, &cst, WNOHANG);
      if (r == cl) break;
      if (now_ms() - t0 > 180000) { FAIL("%s did not finish in 180 s", k->client); kill(cl, SIGKILL); waitpid(cl, &cst, 0); break; }
      usleep(20000);
   }
   drain_into(cp[0], cout_, sizeof(cout_), &coutlen);
   if (!WIFEXITED(cst) || WEXITSTATUS(cst) != 0) FAIL("%s exited with status %d (exit %d)", k->client, cst, WIFEXITED(cst) ? WEXITSTATUS(cst) : -1);
   else printf("gui_comp_test: %s finished (%ld ms)\n", k->client, now_ms() - t0);
   for (int i = 0; k->client_must_print[i]; i++)
      if (!strstr(cout_, k->client_must_print[i])) FAIL("the client never printed \"%s\"", k->client_must_print[i]);
   if (k->want_resizes) {
      unsigned n = 0;
      for (const char *q = cout_; (q = strstr(q, "SNAKE3D resized to ")); q += 10) n++;
      if (n != k->want_resizes) FAIL("%u resizes in the client's output, expected %u", n, k->want_resizes);
      else printf("gui_comp_test: the client was resized %u times\n", n);
      /* fullscreen is the screen (640x360 headless), and back is the window's own size */
      if (!strstr(cout_, "resized to 640x360")) FAIL("F11 did not take the window to the screen's size");
   }
   if (strstr(cout_, "FAIL") || strstr(cout_, "ASSERT")) FAIL("the client reported a problem");

   /* give the compositor a moment to see the client go, then ask it to quit */
   t0 = now_ms();
   while (!strstr(out, " gone") && now_ms() - t0 < 10000) { drain(p[0]); usleep(20000); }
   kill(comp, SIGTERM);
   int kst = 0;
   t0 = now_ms();
   for (;;) {
      drain(p[0]);
      pid_t r = waitpid(comp, &kst, WNOHANG);
      if (r == comp) break;
      if (now_ms() - t0 > 30000) { FAIL("vk_comp did not quit on SIGTERM"); kill(comp, SIGKILL); waitpid(comp, &kst, 0); break; }
      usleep(20000);
   }
   drain(p[0]);
   if (!WIFEXITED(kst) || WEXITSTATUS(kst) != 0) FAIL("vk_comp exited with status %d", kst);

   unsigned long frames = 0, imports = 0, drops = 0, uploads = 0;
   char *q = strstr(out, "COMP quit after");
   if (!q || sscanf(q, "COMP quit after %lu frames (up to %*u draws), %lu imports, %lu drops, %lu uploads", &frames, &imports, &drops, &uploads) != 4) {
      FAIL("no summary from vk_comp:\n%s", out);
   } else {
      printf("gui_comp_test: vk_comp composed %lu frames, imported %lu buffers, dropped %lu, uploaded %lu\n", frames, imports, drops, uploads);
      if (frames < k->min_frames) FAIL("only %lu frames composed (at least %u expected): the client is not paced by the compositor, or the compositor drops frames", frames, k->min_frames);
      if (imports != k->want_buffers) FAIL("%lu buffers imported, expected %u", imports, k->want_buffers);
      if (drops != k->want_buffers) FAIL("%lu buffers dropped, expected %u", drops, k->want_buffers);
   }
   if (!strstr(out, "client 1 connected")) FAIL("the compositor never saw the client");
   if (strstr(out, "COMP FAIL") || strstr(out, "disconnected for a protocol error") || strstr(out, "import of buffer")) FAIL("the compositor reported a problem:\n%s", out);
   if (k->comp_env[0] && !strstr(out, "COMP F11 at frame")) FAIL("the compositor never pressed F11");
   if (k->comp_args[0]) {
      if (!strstr(out, "COMP started")) FAIL("the compositor never started its program");
      if (!strstr(out, "COMP ended 1 program(s) still running")) FAIL("the program the compositor started was not ended with it");
      if (!strstr(out, "client 2 connected")) FAIL("the compositor's own program never connected as a client");
   }
   unlink(SOCK);
   if (failures != before) {
      printf("gui_comp_test: what vk_comp printed (tail):\n%s\n", outlen > 3000 ? out + outlen - 3000 : out);
      printf("gui_comp_test: what the client printed (tail):\n%s\n", coutlen > 3000 ? cout_ + coutlen - 3000 : cout_);
   }
   close(p[0]);
   close(cp[0]);
   return failures != before;
}

int main(void) {
   setvbuf(stdout, NULL, _IONBF, 0);
   for (unsigned i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) run_case(&cases[i]);
   if (failures) {
      printf("gui_comp_test: %d failure(s)\n", failures);
      return 1;
   }
   printf("gui_comp_test: DONE\n");
   return 0;
}
