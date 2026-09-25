// fb0_test: /dev/fb0 — the compositor's screen. Phase 2.1 of
// docs/gui/gui-plan.md.
//
//   1. Exclusive: a second open is EBUSY, and FBIO_BLIT on the console
//      (/dev/fb) is EBUSY while graphics mode is on.
//   2. /proc/fbinfo says "mode: graphics" while it is open, "mode: text"
//      after close.
//   3. FBIO_GET_INFO + mmap(MAP_SHARED) + draw + FBIO_FLUSH. What reaches
//      the screen is checked from the host with a screendump: run
//      `fb0_test hold` and it keeps the picture up for 5 s (see the
//      layout in draw()).
//   4. The screen comes back when the holder dies: after exit and after
//      SIGKILL, a new open succeeds and fbinfo says text.
//   5. The frames are the framebuffer's, not the mapper's: mapping every
//      page, touching it and unmapping (and a child doing the same and
//      exiting) must not hand them to the allocator — MemFree does not
//      jump by the size of the screen.
//   6. The mapping outlives the fd: after close, writing through it does
//      not fault.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/wait.h>

#define FBIO_BLIT     0x46420001
#define FBIO_GET_INFO 0x46420010
#define FBIO_FLUSH    0x46420011

struct fb0_info {
    uint32_t width, height, stride, bytes_per_pixel;
    uint64_t offset, map_len;
};
struct fb0_rect { uint32_t x, y, w, h; };
struct fb0_flush {
    uint32_t count, pad;
    struct fb0_rect rects[16];
};
struct fb_blit { uint64_t ptr; uint32_t width, height; };

static int fails;

static void check(int ok, const char *what) {
    printf("%s: %s\n", what, ok ? "PASS" : "FAIL");
    if (!ok) fails++;
}

static int graphics_mode(void) {
    FILE *f = fopen("/proc/fbinfo", "r");
    if (!f) return -1;
    char line[160];
    int mode = -1;
    while (fgets(line, sizeof line, f)) {
        if (strncmp(line, "mode: graphics", 14) == 0) mode = 1;
        else if (strncmp(line, "mode: text", 10) == 0) mode = 0;
    }
    fclose(f);
    return mode;
}

static long mem_free_kb(void) {
    FILE *f = fopen("/proc/meminfo", "r");
    if (!f) return -1;
    char line[128];
    long kb = -1;
    while (fgets(line, sizeof line, f))
        if (sscanf(line, "MemFree: %ld kB", &kb) == 1) break;
    fclose(f);
    return kb;
}

static int wait_child(pid_t pid) {
    int st = 0;
    if (waitpid(pid, &st, 0) != pid) return 1000;
    if (WIFSIGNALED(st)) return -WTERMSIG(st);
    return WIFEXITED(st) ? WEXITSTATUS(st) : 1001;
}

static void fill(uint8_t *base, const struct fb0_info *in, int x0, int y0, int w, int h, uint32_t rgb) {
    for (int y = y0; y < y0 + h && y < (int)in->height; y++) {
        uint32_t *row = (uint32_t *)(base + in->offset + (size_t)y * in->stride * 4);
        for (int x = x0; x < x0 + w && x < (int)in->width; x++) row[x] = rgb;
    }
}

// The picture `hold` leaves up, for the host to check pixel by pixel:
// background 0x203060, red square (100,100)-(299,299), green square
// (400,100)-(599,299). The green one is flushed by its own rect, the rest
// by one full-screen rect, so both FBIO_FLUSH shapes are exercised.
static int draw(int fd, uint8_t *base, const struct fb0_info *in) {
    fill(base, in, 0, 0, in->width, in->height, 0x203060);
    fill(base, in, 100, 100, 200, 200, 0xff0000);
    struct fb0_flush fl = { .count = 1, .rects = { { 0, 0, in->width, in->height } } };
    if (ioctl(fd, FBIO_FLUSH, &fl) != 0) return -1;
    fill(base, in, 400, 100, 200, 200, 0x00ff00);
    struct fb0_flush fl2 = { .count = 1, .rects = { { 400, 100, 200, 200 } } };
    return ioctl(fd, FBIO_FLUSH, &fl2);
}

int main(int argc, char **argv) {
    int hold = argc > 1 && strcmp(argv[1], "hold") == 0;

    // 1 + 2
    check(graphics_mode() == 0, "2 text mode before open");
    int fd = open("/dev/fb0", O_RDWR);
    check(fd >= 0, "1 open /dev/fb0");
    if (fd < 0) { printf("errno=%d\n", errno); return 1; }
    int fd2 = open("/dev/fb0", O_RDWR);
    check(fd2 < 0 && errno == EBUSY, "1 second open is EBUSY");
    check(graphics_mode() == 1, "2 graphics mode while open");
    uint32_t px[4] = {0};
    struct fb_blit b = { (uint64_t)(uintptr_t)px, 2, 2 };
    int con = open("/dev/fb", O_WRONLY);
    check(ioctl(con, FBIO_BLIT, &b) < 0 && errno == EBUSY, "1 FBIO_BLIT on /dev/fb is EBUSY");
    close(con);

    // 3
    struct fb0_info in;
    int ok = ioctl(fd, FBIO_GET_INFO, &in) == 0;
    printf("  %ux%u stride=%u bpp=%u offset=%llu map_len=%llu\n", in.width, in.height, in.stride,
           in.bytes_per_pixel, (unsigned long long)in.offset, (unsigned long long)in.map_len);
    ok &= in.bytes_per_pixel == 4 && in.stride >= in.width && in.map_len >= in.offset + (uint64_t)in.stride * in.height * 4;
    check(ok, "3 FBIO_GET_INFO");
    uint8_t *base = mmap(NULL, in.map_len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    check(base != MAP_FAILED, "3 mmap /dev/fb0");
    if (base == MAP_FAILED) return 1;
    check(draw(fd, base, &in) == 0, "3 draw + FBIO_FLUSH");
    struct fb0_flush bad = { .count = 17 };
    check(ioctl(fd, FBIO_FLUSH, &bad) < 0 && errno == EINVAL, "3 FBIO_FLUSH with 17 rects is EINVAL");
    if (hold) {
        printf("fb0_test: HOLDING\n");
        fflush(stdout);
        sleep(5);
    }

    // 6
    close(fd);
    check(graphics_mode() == 0, "2 text mode after close");
    base[in.offset] = 1; // must not fault
    check(1, "6 mapping outlives the fd");
    munmap(base, in.map_len);

    // 4
    fd = open("/dev/fb0", O_RDWR);
    check(fd >= 0, "4 reopen after close");
    close(fd);
    pid_t pid = fork();
    if (pid == 0) { if (open("/dev/fb0", O_RDWR) < 0) _exit(1); _exit(0); }
    check(wait_child(pid) == 0 && graphics_mode() == 0, "4 text mode after the holder exits");
    int p[2];
    pipe(p);
    pid = fork();
    if (pid == 0) {
        int f = open("/dev/fb0", O_RDWR);
        write(p[1], f >= 0 ? "y" : "n", 1);
        for (;;) pause();
    }
    char c = 0;
    read(p[0], &c, 1);
    int was = graphics_mode();
    kill(pid, SIGKILL);
    int st = wait_child(pid);
    check(c == 'y' && was == 1 && st == -SIGKILL && graphics_mode() == 0, "4 text mode after the holder is SIGKILLed");
    fd = open("/dev/fb0", O_RDWR);
    check(fd >= 0, "4 reopen after SIGKILL");

    // 5
    long before = mem_free_kb();
    for (int i = 0; i < 3; i++) {
        uint8_t *m = mmap(NULL, in.map_len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (m == MAP_FAILED) break;
        for (uint64_t off = 0; off < in.map_len; off += 4096) m[off] |= 0;
        munmap(m, in.map_len);
    }
    pid = fork();
    if (pid == 0) {
        uint8_t *m = mmap(NULL, in.map_len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (m == MAP_FAILED) _exit(1);
        for (uint64_t off = 0; off < in.map_len; off += 4096) m[off] |= 0;
        _exit(0);
    }
    int cst = wait_child(pid);
    long after = mem_free_kb();
    printf("  MemFree before=%ld kB after=%ld kB (screen is %llu kB)\n", before, after,
           (unsigned long long)in.map_len / 1024);
    check(cst == 0 && after - before < (long)(in.map_len / 1024) / 2, "5 unmapping never frees the screen's frames");
    close(fd);

    printf("fb0_test: %s (%d failed)\n", fails ? "FAIL" : "PASS", fails);
    return fails ? 1 : 0;
}
