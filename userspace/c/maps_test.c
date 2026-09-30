// /proc/self/maps: one line per VMA in Linux's format, showing mmap, mprotect splits, munmap and the stack.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>

#define printf(...) ((printf)(__VA_ARGS__), fflush(stdout))
static int failures;
static long mprot(void *a, size_t n, int prot) {
    long r;
    __asm__ volatile("syscall" : "=a"(r) : "a"(10L), "D"(a), "S"(n), "d"((long)prot) : "rcx", "r11", "memory");
    return r;
}
// `perms` starts with `want` (2 chars: r and w); the x bit is not compared, the kernel maps anonymous memory executable (no NX).
static int has(const char *perms, const char *want) { return strncmp(perms, want, 2) == 0 && perms[3] == 'p'; }
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

static char maps[16384];
static int load(void) {
    FILE *f = fopen("/proc/self/maps", "r");
    if (!f) return -1;
    size_t n = fread(maps, 1, sizeof maps - 1, f);
    maps[n] = 0;
    fclose(f);
    int lines = 0;
    for (char *p = maps; *p; p++) if (*p == '\n') lines++;
    return lines;
}
// The permission string of the line that contains `addr` (adjacent anonymous mappings merge, so it may start earlier), or "" if none
// does; `end_out` gets that line's end.
static const char *perms_at(unsigned long addr, unsigned long *end_out) {
    static char out[8];
    for (char *l = maps; *l;) {
        unsigned long s, e;
        if (sscanf(l, "%lx-%lx %4s", &s, &e, out) == 3 && addr >= s && addr < e) {
            if (end_out) *end_out = e;
            return out;
        }
        while (*l && *l != '\n') l++;
        if (*l) l++;
    }
    return "";
}

int main(void) {
    printf("format\n");
    int base = load();
    CHECK(base > 3, "only %d lines", base);
    unsigned long s, e; char perms[8], name[32] = {0}; unsigned long off; int a, b, ino;
    CHECK(sscanf(maps, "%lx-%lx %4s %lx %x:%x %d", &s, &e, perms, &off, &a, &b, &ino) == 7 && e > s, "the first line does not parse: %.60s", maps);
    CHECK(strstr(maps, "[stack]") != NULL, "no [stack] line");
    (void)name;

    printf("mmap, mprotect split, munmap\n");
    char *m = mmap(NULL, 3 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    unsigned long at = (unsigned long)m;
    load();
    unsigned long end = 0;
    const char *p = perms_at(at, &end);
    CHECK(has(p, "rw") && end >= at + 3 * 4096, "the new mapping reads '%s' ending %lx", p, end);
    int with = load();
    mprot(m + 4096, 4096, PROT_NONE);
    int after = load();
    CHECK(has(perms_at(at, &end), "rw") && end == at + 4096, "first page: '%s' to %lx", perms_at(at, &end), end);
    CHECK(strcmp(perms_at(at + 4096, &end), "---p") == 0 && end == at + 2 * 4096, "middle page PROT_NONE: '%s'", perms_at(at + 4096, &end));
    CHECK(has(perms_at(at + 2 * 4096, &end), "rw"), "last page: '%s'", perms_at(at + 2 * 4096, &end));
    CHECK(after == with + 2, "the split added %d lines, wanted 2", after - with);
    mprot(m, 4096, PROT_READ | PROT_EXEC);
    load();
    CHECK(has(perms_at(at, &end), "r-"), "PROT_READ|PROT_EXEC reads '%s' (write must be off)", perms_at(at, &end));
    munmap(m, 3 * 4096);
    CHECK(load() <= base + 1 && strcmp(perms_at(at, NULL), "") == 0, "munmap left its lines behind");

    printf("shared mappings and another process\n");
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/maps", getpid());
    FILE *f = fopen(path, "r");
    CHECK(f != NULL, "/proc/<pid>/maps does not open");
    if (f) fclose(f);
    CHECK(fopen("/proc/999999/maps", "r") == NULL, "a missing process must not open");

    printf(failures ? "maps_test: FAIL\n" : "maps_test: PASS\n");
    return failures != 0;
}
