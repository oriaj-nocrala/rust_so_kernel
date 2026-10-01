// gui_comp_test: the GPU compositor (vk_comp) with a real client (vk_window) in QEMU, on the software device (G5 layer 4, slice 3c).
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

/* Reads whatever the compositor printed (its pipe is non-blocking). */
static void drain(int fd) {
   for (;;) {
      long n = read(fd, out + outlen, sizeof(out) - 1 - outlen);
      if (n <= 0) break;
      outlen += (size_t)n;
      out[outlen] = 0;
      if (outlen >= sizeof(out) - 1) { outlen = 0; }   /* keep the tail only: the summary is at the end */
   }
}

int main(void) {
   setvbuf(stdout, NULL, _IONBF, 0);
   unlink(SOCK);
   int p[2];
   if (pipe(p) != 0) { printf("gui_comp_test FAIL: pipe\n"); return 1; }
   pid_t comp = fork();
   if (comp == 0) {
      dup2(p[1], 1);
      dup2(p[1], 2);
      close(p[0]);
      setenv("COMP_HEADLESS", "1", 1);
      setenv("COMP_NO_INPUT", "1", 1);
      setenv("COMP_SOCKET", SOCK, 1);
      char *argv[] = { "/mnt/bin/vk_comp", NULL };
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

   pid_t cl = fork();
   if (cl == 0) {
      setenv("GUI_DISPLAY", SOCK, 1);
      char *argv[] = { "/mnt/bin/vk_window", NULL };
      execv(argv[0], argv);
      _exit(127);
   }
   int cst = 0;
   t0 = now_ms();
   for (;;) {
      drain(p[0]);
      pid_t r = waitpid(cl, &cst, WNOHANG);
      if (r == cl) break;
      if (now_ms() - t0 > 120000) { FAIL("vk_window did not finish in 120 s"); kill(cl, SIGKILL); waitpid(cl, &cst, 0); break; }
      usleep(20000);
   }
   if (!WIFEXITED(cst) || WEXITSTATUS(cst) != 0) FAIL("vk_window exited with status %d (exit %d)", cst, WIFEXITED(cst) ? WEXITSTATUS(cst) : -1);
   else printf("gui_comp_test: vk_window finished (%ld ms)\n", now_ms() - t0);

   /* give the compositor a moment to see the client go, then ask it to quit */
   t0 = now_ms();
   while (!strstr(out, "client 1 gone") && now_ms() - t0 < 10000) { drain(p[0]); usleep(20000); }
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
      if (frames < 10) FAIL("only %lu frames composed", frames);
      if (imports != 6) FAIL("%lu buffers imported, expected 6 (two swapchains of three)", imports);
      if (drops != 6) FAIL("%lu buffers dropped, expected 6", drops);
   }
   if (!strstr(out, "client 1 connected")) FAIL("the compositor never saw the client");
   if (strstr(out, "COMP FAIL") || strstr(out, "disconnected for a protocol error") || strstr(out, "import of buffer")) FAIL("the compositor reported a problem:\n%s", out);
   unlink(SOCK);
   if (failures) {
      printf("gui_comp_test: what vk_comp printed (tail):\n%s\n", outlen > 3000 ? out + outlen - 3000 : out);
      printf("gui_comp_test: %d failure(s)\n", failures);
      return 1;
   }
   printf("gui_comp_test: DONE\n");
   return 0;
}
