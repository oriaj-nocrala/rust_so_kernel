// mprotect(10), partial munmap(11) and PROT_NONE. Raw `syscall` on purpose: this checks the kernel's ABI, not mlibc's wrappers.
// Faults are observed from a forked child (the kernel kills it with SIGSEGV). See docs/reference/memory.md.
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <sys/wait.h>

static long sc(long nr, long a, long b, long c, long d, long e, long f) {
    long ret;
    register long r10 __asm__("r10") = d;
    register long r8 __asm__("r8") = e;
    register long r9 __asm__("r9") = f;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8), "r"(r9) : "rcx", "r11", "memory");
    return ret;
}

enum { SYS_mmap = 9, SYS_mprotect = 10, SYS_munmap = 11 };
enum { R = 1, W = 2, RW = 3 };
enum { MAP_PRIVATE_ANON = 0x22 };
enum { EINVAL_ = 22, ENOMEM_ = 12 };
#define PG 4096UL

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

static char *map(unsigned long len, int prot) {
    long r = sc(SYS_mmap, 0, len, prot, MAP_PRIVATE_ANON, -1, 0);
    return (r < 0 && r > -4096) ? NULL : (char *)r;
}

enum access { READ, WRITE };

// Touch `p` in a child; true if the child survived, false if it was killed by SIGSEGV.
static int survives(volatile char *p, enum access how) {
    pid_t pid = fork();
    if (pid == 0) {
        if (how == WRITE) *p = 1; else (void)*p;
        _exit(0);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    return WIFEXITED(st) && WEXITSTATUS(st) == 0;
}

// A child maps a fresh page, writes it (the PTE is present and writable, nothing inherited from a fork), lowers it to
// `prot`, then touches it: true if that survived. This is the check that the PTE itself follows mprotect.
static int survives_lowered(int prot, enum access how) {
    pid_t pid = fork();
    if (pid == 0) {
        char *p = map(PG, RW);
        p[0] = 1;
        if (sc(SYS_mprotect, (long)p, PG, prot, 0, 0, 0) != 0) _exit(2);
        if (how == WRITE) p[0] = 2; else (void)*(volatile char *)p;
        _exit(0);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    return WIFEXITED(st) && WEXITSTATUS(st) == 0;
}

static void test_lowered(void) {
    printf("lowering a present page\n");
    CHECK(survives_lowered(RW, WRITE), "control: RW -> RW, write");
    CHECK(survives_lowered(R, READ), "R: read");
    CHECK(!survives_lowered(R, WRITE), "R: write");
    CHECK(!survives_lowered(0, READ), "NONE: read");
    CHECK(!survives_lowered(0, WRITE), "NONE: write");
}

static void test_prot_none(void) {
    printf("PROT_NONE\n");
    char *p = map(4 * PG, 0);
    CHECK(p != NULL, "mmap PROT_NONE");
    CHECK(!survives(p, READ), "read of a never-touched PROT_NONE page");
    CHECK(!survives(p, WRITE), "write to a PROT_NONE page");
    CHECK(sc(SYS_mprotect, (long)p, 4 * PG, RW, 0, 0, 0) == 0, "mprotect RW");
    p[0] = 7; p[3 * PG] = 9;
    CHECK(p[0] == 7 && p[3 * PG] == 9 && p[PG] == 0, "usable after mprotect");
    CHECK(sc(SYS_mprotect, (long)p, 4 * PG, 0, 0, 0, 0) == 0, "mprotect back to NONE");
    CHECK(!survives(p, READ), "read of a touched page after PROT_NONE");
    CHECK(!survives(p + 3 * PG, WRITE), "write to a touched page after PROT_NONE");
    CHECK(sc(SYS_mprotect, (long)p, 4 * PG, RW, 0, 0, 0) == 0, "mprotect RW again");
    CHECK(p[0] == 7 && p[3 * PG] == 9, "contents kept across NONE");
    sc(SYS_munmap, (long)p, 4 * PG, 0, 0, 0, 0);
}

// The shape of a thread stack: reserve NONE, then make all but a guard page usable.
static void test_guard(void) {
    printf("guard page\n");
    char *p = map(2 * 1024 * 1024 + 2 * PG, 0);
    CHECK(p != NULL, "mmap 2 MiB + 8 KiB PROT_NONE");
    CHECK(sc(SYS_mprotect, (long)(p + PG), 2 * 1024 * 1024 + PG, RW, 0, 0, 0) == 0, "mprotect all but the guard");
    p[PG] = 1;
    p[2 * 1024 * 1024 + PG] = 2;
    CHECK(p[PG] == 1 && p[2 * 1024 * 1024 + PG] == 2, "both ends usable");
    CHECK(!survives(p, WRITE), "guard page faults");
    CHECK(survives(p + PG, WRITE), "first usable page does not");
    CHECK(sc(SYS_munmap, (long)p, 2 * 1024 * 1024 + 2 * PG, 0, 0, 0, 0) == 0, "munmap the lot");
}

static void test_read_only(void) {
    printf("read-only\n");
    char *p = map(3 * PG, RW);
    memset(p, 0x5a, 3 * PG);
    CHECK(sc(SYS_mprotect, (long)(p + PG), PG, R, 0, 0, 0) == 0, "mprotect the middle page R");
    CHECK(survives(p, WRITE), "page before is still writable");
    CHECK(!survives(p + PG, WRITE), "write to the read-only middle page");
    CHECK(survives(p + PG, READ), "read of the middle page");
    CHECK(survives(p + 2 * PG, WRITE), "page after is still writable");
    CHECK(p[PG] == 0x5a, "middle page keeps its data");
    CHECK(sc(SYS_mprotect, (long)(p + PG), PG, RW, 0, 0, 0) == 0, "mprotect back to RW");
    p[PG] = 0x11;
    CHECK(p[PG] == 0x11, "writable again");
    // a page that was only ever read maps the zero frame; raising to RW must not write through it
    char *z = map(PG, R);
    CHECK(z[0] == 0, "reads zero");
    CHECK(sc(SYS_mprotect, (long)z, PG, RW, 0, 0, 0) == 0, "R -> RW on a zero-frame page");
    z[0] = 5;
    char *z2 = map(PG, R);
    CHECK(z2[0] == 0 && z[0] == 5, "the shared zero frame was not written");
    sc(SYS_munmap, (long)p, 3 * PG, 0, 0, 0, 0);
    sc(SYS_munmap, (long)z, PG, 0, 0, 0, 0);
    sc(SYS_munmap, (long)z2, PG, 0, 0, 0, 0);
}

// A child that reads a page the parent has made read-only sees the parent's data, and a child's write does not reach the parent.
static void test_fork_cow(void) {
    printf("fork + mprotect\n");
    char *p = map(2 * PG, RW);
    p[0] = 1; p[PG] = 2;
    CHECK(sc(SYS_mprotect, (long)p, PG, R, 0, 0, 0) == 0, "first page R");
    pid_t pid = fork();
    if (pid == 0) {
        sc(SYS_mprotect, (long)p, PG, RW, 0, 0, 0);
        p[0] = 100; p[PG] = 101;
        _exit(p[0] == 100 && p[PG] == 101 ? 0 : 1);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "child status %d", st);
    CHECK(p[0] == 1 && p[PG] == 2, "parent kept %d %d", p[0], p[PG]);
    sc(SYS_munmap, (long)p, 2 * PG, 0, 0, 0, 0);
}

static void test_partial_munmap(void) {
    printf("partial munmap\n");
    char *p = map(8 * PG, RW);
    for (int i = 0; i < 8; i++) p[i * PG] = (char)(i + 1);
    CHECK(sc(SYS_munmap, (long)(p + 2 * PG), 2 * PG, 0, 0, 0, 0) == 0, "punch a hole in the middle");
    CHECK(p[0] == 1 && p[PG] == 2 && p[4 * PG] == 5 && p[7 * PG] == 8, "both sides intact");
    CHECK(!survives(p + 2 * PG, READ), "the hole faults");
    CHECK(!survives(p + 3 * PG, WRITE), "the hole faults (2)");
    CHECK(sc(SYS_munmap, (long)p, 2 * PG, 0, 0, 0, 0) == 0, "cut off the head");
    CHECK(sc(SYS_munmap, (long)(p + 6 * PG), 2 * PG, 0, 0, 0, 0) == 0, "cut off the tail");
    CHECK(p[4 * PG] == 5 && p[5 * PG] == 6, "what is left is intact");
    CHECK(sc(SYS_munmap, (long)p, 8 * PG, 0, 0, 0, 0) == 0, "one munmap over pieces and holes");
    CHECK(!survives(p + 4 * PG, READ), "all gone");
    CHECK(sc(SYS_munmap, (long)p, 8 * PG, 0, 0, 0, 0) == 0, "munmap of nothing succeeds");
    // the frames really came back: map and drop the same amount many times
    for (int i = 0; i < 200; i++) {
        char *q = map(64 * PG, RW);
        for (int j = 0; j < 64; j++) q[j * PG] = 1;
        sc(SYS_munmap, (long)(q + 10 * PG), 20 * PG, 0, 0, 0, 0);
        sc(SYS_munmap, (long)q, 64 * PG, 0, 0, 0, 0);
    }
    CHECK(1, "200 rounds of map / punch / unmap");
}

static void test_errors(void) {
    printf("errors\n");
    char *p = map(4 * PG, RW);
    CHECK(sc(SYS_mprotect, (long)p + 1, PG, R, 0, 0, 0) == -EINVAL_, "unaligned addr");
    CHECK(sc(SYS_mprotect, (long)p, PG, 8, 0, 0, 0) == -EINVAL_, "unknown prot bit");
    CHECK(sc(SYS_mprotect, (long)p, 0, R, 0, 0, 0) == 0, "length 0");
    CHECK(sc(SYS_mprotect, (long)p, 8 * PG, R, 0, 0, 0) == -ENOMEM_, "range runs past the mapping");
    CHECK(sc(SYS_mprotect, 0x10000000000UL, PG, R, 0, 0, 0) == -ENOMEM_, "unmapped range");
    CHECK(p[0] == 0 && survives(p, WRITE), "a failed mprotect changed nothing");
    CHECK(sc(SYS_munmap, (long)p + 1, PG, 0, 0, 0, 0) == -EINVAL_, "munmap unaligned");
    CHECK(sc(SYS_munmap, (long)p, 0, 0, 0, 0, 0) == -EINVAL_, "munmap length 0");
    sc(SYS_munmap, (long)p, 4 * PG, 0, 0, 0, 0);
}

// One page at a time made read-only, then writable again: a range that ends up uniform must be one VMA again,
// or the list (256 entries) runs dry long before the last page.
static void test_merge(void) {
    printf("merge\n");
    enum { N = 300 };
    char *p = map(N * PG, RW);
    int ok = 1;
    for (int i = 0; i < N; i++) ok &= sc(SYS_mprotect, (long)(p + i * PG), PG, R, 0, 0, 0) == 0;
    CHECK(ok, "%d single-page mprotects R", N);
    for (int i = 0; i < N; i++) ok &= sc(SYS_mprotect, (long)(p + i * PG), PG, RW, 0, 0, 0) == 0;
    CHECK(ok, "%d single-page mprotects back to RW", N);
    for (int i = 0; i < N; i++) p[i * PG] = (char)i;
    ok = 1;
    for (int i = 0; i < N; i++) ok &= p[i * PG] == (char)i;
    CHECK(ok, "all pages usable");
    CHECK(sc(SYS_munmap, (long)p, N * PG, 0, 0, 0, 0) == 0, "one munmap for all of it");
}

int main(void) {
    test_prot_none();
    test_lowered();
    test_guard();
    test_read_only();
    test_fork_cow();
    test_partial_munmap();
    test_errors();
    test_merge();
    if (failures) {
        printf("mprotect_test: %d FAILED\n", failures);
        return 1;
    }
    printf("mprotect_test: OK\n");
    return 0;
}
