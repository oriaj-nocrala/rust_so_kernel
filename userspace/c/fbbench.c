// fbbench — fixed framebuffer-console workloads, measured by the kernel's
// own counters (/proc/fbinfo) and by wall-clock time.
//
// Phase 0 of docs/fb/wc-shadow-plan.md. The console is fast in QEMU and
// slow on the physical AM4 machine, and every later phase of that plan
// (PAT reprogramming, WC mapping, shadow buffer) is judged by numbers
// from the machine. "Before" and "after" must therefore be the same
// binary running the same workload, not a hand-timed `seq 1 400`.
//
// Workloads, each bracketed by a /proc/fbinfo snapshot:
//   S  seq:        what `seq 1 400` writes, one write() per line. The
//                  reference load of docs/fb/console-perf.md (there 330
//                  scrolls and 1616 glyphs; here it starts from the last
//                  row, so exactly 400 scrolls). Dominated by scrolling
//   A  scroll:     400 lines of 80 columns, one write() per line, so
//                  32000 glyphs. Dominated by draw_char, not scrolling
//   A1 scroll-1w:  the same 400 lines in a single write(). Phase 3 flushes
//                  once per write(), so A vs A1 shows that effect
//   B  backspace:  100x "rewrite a long line, then \b + ESC[J", which is
//                  what ash emits when you delete a character
//   C  blit:       30 FBIO_BLIT frames of 320x200, DOOM's size
//
// Workloads write to their own /dev/fb descriptor, so the report can go
// to stdout, and `fbbench > /tmp/fbbench.txt` works too. The report is
// printed only after every workload has run: printing in between would
// add its own console cost to the next snapshot. It fits on one screen
// so it can be photographed, and it lands in the USB log after
// `kdebug sync`.
//
// Per-operation lines are deltas of the kernel's diag::OpStat counters:
// calls, total cycles, cycles per call, and MB/s. Cycles are wall-clock
// TSC deltas, so a timer preemption inside a measured call is charged to
// it (see diag/src/fbstat.rs). Compare totals under the same workload,
// not single calls. fb_render_bytes counts input bytes, not
// framebuffer bytes, so its MB/s is console throughput and not bus
// bandwidth.
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

// Custom, this-kernel-only ioctl. See sys_ioctl's FBIO_BLIT in
// kernel/src/process/syscall/fs.rs.
#define FBIO_BLIT 0x46420001UL

struct fb_blit_args {
    uint64_t ptr;
    uint32_t width;
    uint32_t height;
};

// Order and names match debug::render_fb_report.
static const char *const OPS[] = {
    "fb_fill_rect", "fb_draw_char", "fb_scroll_up", "fb_cursor_xor",
    "fb_blit_scaled", "fb_render_bytes", "fb_serial_mirror",
};
#define NOPS (sizeof(OPS) / sizeof(OPS[0]))

struct op_counts {
    unsigned long long calls, bytes, cycles;
};

struct snapshot {
    struct op_counts op[NOPS];
};

static unsigned long long tsc_hz;
static char pte_line[160];
static char mtrr_line[160];

static char fbinfo_buf[16384];

static const char *read_fbinfo(void) {
    int fd = open("/proc/fbinfo", O_RDONLY);
    if (fd < 0) return NULL;
    size_t len = 0;
    for (;;) {
        ssize_t n = read(fd, fbinfo_buf + len, sizeof(fbinfo_buf) - 1 - len);
        if (n <= 0) break;
        len += (size_t)n;
        if (len == sizeof(fbinfo_buf) - 1) break;
    }
    close(fd);
    fbinfo_buf[len] = '\0';
    return fbinfo_buf;
}

static void copy_line(char *dst, size_t cap, const char *line) {
    size_t n = strcspn(line, "\n");
    if (n >= cap) n = cap - 1;
    memcpy(dst, line, n);
    dst[n] = '\0';
}

static int take_snapshot(struct snapshot *s) {
    memset(s, 0, sizeof(*s));
    const char *text = read_fbinfo();
    if (!text) return -1;
    for (const char *line = text; *line; ) {
        for (size_t i = 0; i < NOPS; i++) {
            size_t k = strlen(OPS[i]);
            if (strncmp(line, OPS[i], k) == 0 && line[k] == ':') {
                // "name: calls=N bytes=N cycles=N (...)", or just
                // "name: calls=0" before the first call.
                sscanf(line + k + 1, " calls=%llu bytes=%llu cycles=%llu",
                       &s->op[i].calls, &s->op[i].bytes, &s->op[i].cycles);
            }
        }
        if (strncmp(line, "tsc_hz:", 7) == 0) sscanf(line + 7, " %llu", &tsc_hz);
        if (strncmp(line, "pte_cache_bits:", 15) == 0) copy_line(pte_line, sizeof(pte_line), line);
        if (strncmp(line, "mtrr_type:", 10) == 0) copy_line(mtrr_line, sizeof(mtrr_line), line);
        const char *nl = strchr(line, '\n');
        if (!nl) break;
        line = nl + 1;
    }
    return 0;
}

static unsigned long long now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (unsigned long long)ts.tv_sec * 1000000000ULL + (unsigned long long)ts.tv_nsec;
}

static void write_all(int fd, const char *p, size_t n) {
    while (n > 0) {
        ssize_t w = write(fd, p, n);
        if (w <= 0) return;
        p += w;
        n -= (size_t)w;
    }
}

// ── workloads ───────────────────────────────────────────────────────────

#define SCROLL_LINES 400
#define LINE_LEN 81 // 80 columns + '\n'

static char scroll_text[SCROLL_LINES * LINE_LEN];

static void build_scroll_text(void) {
    for (int i = 0; i < SCROLL_LINES; i++) {
        char *l = scroll_text + i * LINE_LEN;
        int n = snprintf(l, LINE_LEN, "%04d ", i + 1);
        for (int c = n; c < LINE_LEN - 1; c++) l[c] = (char)('a' + (i + c) % 26);
        l[LINE_LEN - 1] = '\n';
    }
}

// Run outside the measured bracket: put the cursor on the last row, so
// every line of S scrolls, and so does every line of A after it. Without
// it the scroll count depends on how much of
// the screen the boot log happened to fill. Measured in QEMU: 330
// scrolls instead of 400, from a cursor that started mid-screen.
static void park_cursor_at_bottom(int fd) {
    struct winsize ws;
    int rows = (ioctl(fd, TIOCGWINSZ, &ws) == 0 && ws.ws_row > 0) ? ws.ws_row : 200;
    for (int i = 0; i < rows; i++) write_all(fd, "\n", 1);
}

static void workload_seq(int fd) {
    char line[8];
    for (int i = 1; i <= SCROLL_LINES; i++) {
        int n = snprintf(line, sizeof(line), "%d\n", i);
        write_all(fd, line, (size_t)n);
    }
}

static void workload_scroll(int fd) {
    for (int i = 0; i < SCROLL_LINES; i++) write_all(fd, scroll_text + i * LINE_LEN, LINE_LEN);
}

static void workload_scroll_one_write(int fd) {
    write_all(fd, scroll_text, sizeof(scroll_text));
}

#define BACKSPACE_ROUNDS 100

static void workload_backspace(int fd) {
    // '\r' first so every round redraws the same row, the way ash
    // redraws its own line, instead of wrapping and scrolling.
    char line[80];
    for (int r = 0; r < BACKSPACE_ROUNDS; r++) {
        int n = snprintf(line, sizeof(line), "\r~ # echo round %03d ", r);
        while (n < 72) { line[n] = (char)('a' + n % 26); n++; }
        write_all(fd, line, (size_t)n);
        write_all(fd, "\b\x1b[J", 4);
    }
    write_all(fd, "\n", 1);
}

#define BLIT_W 320
#define BLIT_H 200
// 30, not the 200 the plan first said: in a debug-profile kernel under
// QEMU one blit measured ~1.1 G cycles, so 200 frames took a minute.
// cyc/call is the figure that matters, and 30 is plenty for it.
#define BLIT_FRAMES 30

static uint32_t blit_pixels[BLIT_W * BLIT_H];

static int workload_blit(int fd) {
    struct fb_blit_args args = {
        .ptr = (uint64_t)(uintptr_t)blit_pixels, .width = BLIT_W, .height = BLIT_H,
    };
    for (int f = 0; f < BLIT_FRAMES; f++) {
        // A moving gradient, so no two frames are identical.
        for (int y = 0; y < BLIT_H; y++)
            for (int x = 0; x < BLIT_W; x++)
                blit_pixels[y * BLIT_W + x] =
                    ((uint32_t)((x + f) & 0xFF) << 16) | ((uint32_t)((y + f) & 0xFF) << 8) | (uint32_t)(f & 0xFF);
        if (ioctl(fd, FBIO_BLIT, &args) < 0) return -1;
    }
    return 0;
}

// ── report ──────────────────────────────────────────────────────────────

struct result {
    const char *name;
    const char *desc;
    unsigned long long wall_ns;
    struct snapshot before, after;
    int failed;
};

static void print_result(const struct result *r) {
    if (r->failed) {
        printf("%-3s %-34s FAILED\n", r->name, r->desc);
        return;
    }
    printf("%-3s %-34s %6llu.%03llu ms\n", r->name, r->desc,
           r->wall_ns / 1000000ULL, (r->wall_ns / 1000ULL) % 1000ULL);
    for (size_t i = 0; i < NOPS; i++) {
        unsigned long long calls = r->after.op[i].calls - r->before.op[i].calls;
        unsigned long long bytes = r->after.op[i].bytes - r->before.op[i].bytes;
        unsigned long long cyc = r->after.op[i].cycles - r->before.op[i].cycles;
        if (calls == 0) continue;
        printf("    %-17s calls=%-7llu Mcyc=%-8llu cyc/call=%-9llu", OPS[i], calls,
               cyc / 1000000ULL, cyc / calls);
        if (tsc_hz && cyc && bytes)
            // double, not __int128: bytes * tsc_hz overflows u64 for a
            // few GB moved, and the sysroot has no __udivti3.
            printf(" %llu MB/s\n",
                   (unsigned long long)((double)bytes * (double)tsc_hz / (double)cyc / 1e6));
        else
            printf(" -\n");
    }
}

int main(void) {
    int fb = open("/dev/fb", O_WRONLY);
    if (fb < 0) {
        printf("fbbench: cannot open /dev/fb\n");
        return 1;
    }
    struct snapshot probe;
    if (take_snapshot(&probe) < 0) {
        printf("fbbench: cannot read /proc/fbinfo\n");
        return 1;
    }

    build_scroll_text();

    struct result res[] = {
        {"S", "seq 1 400"},
        {"A", "scroll: 400 lines x 80 cols"},
        {"A1", "scroll: same 400 lines, 1 write()"},
        {"B", "backspace: 100x line + \\b ESC[J"},
        {"C", "blit: 30 frames 320x200"},
    };
    const size_t n = sizeof(res) / sizeof(res[0]);

    for (size_t i = 0; i < n; i++) {
        struct result *r = &res[i];
        if (i == 0) park_cursor_at_bottom(fb);
        take_snapshot(&r->before);
        unsigned long long t0 = now_ns();
        switch (i) {
        case 0: workload_seq(fb); break;
        case 1: workload_scroll(fb); break;
        case 2: workload_scroll_one_write(fb); break;
        case 3: workload_backspace(fb); break;
        case 4: r->failed = workload_blit(fb) < 0; break;
        }
        r->wall_ns = now_ns() - t0;
        take_snapshot(&r->after);
    }
    close(fb);

    // After C the console clears itself on its next text write
    // (FB_RAW_DIRTY), so this report starts on a clean screen.
    printf("fbbench  tsc_hz=%llu\n%s\n%s\n", tsc_hz, pte_line, mtrr_line);
    for (size_t i = 0; i < n; i++) print_result(&res[i]);
    return 0;
}
