// pread64 (17) / pwrite64 (18): data at an offset, the position left where it was, on a file in /tmp (ramfs, the generic
// seek-and-back path), on /mnt (ext2) and on a memfd (its own path: the shared position is never touched, a dup sees it
// unmoved); a write past the end grows the file; ESPIPE on a pipe, EINVAL for a negative offset, EBADF for a closed fd;
// the rights are Capsicum's CAP_PREAD/CAP_PWRITE (read or write plus seek); allowed in capability mode. Raw syscalls.
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>
#include "constanos_capsicum.h"

static long sc(long nr, long a, long b, long c, long d) {
    long ret;
    register long r10 __asm__("r10") = d;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10) : "rcx", "r11", "memory");
    return ret;
}

enum {
    SYS_read = 0, SYS_write = 1, SYS_close = 3, SYS_lseek = 8, SYS_pread64 = 17, SYS_pwrite64 = 18, SYS_pipe = 22,
    SYS_dup = 32, SYS_fork = 57, SYS_exit = 60, SYS_wait4 = 61, SYS_unlink = 87, SYS_openat = 257, SYS_memfd_create = 319,
    AT_FDCWD_ = -100, O_RDWR_ = 2, O_CREAT_ = 0100, O_TRUNC_ = 01000,
    EBADF_ = 9, EINVAL_ = 22, ESPIPE_ = 29,
};

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

static long pos(long fd) { return sc(SYS_lseek, fd, 0, SEEK_CUR, 0); }

// The same checks on any seekable fd holding "hello world" with its position at 3.
static void on(const char *what, long fd) {
    printf("%s:\n", what);
    char buf[16] = {0};
    long n = sc(SYS_pread64, fd, (long)buf, 5, 6);
    CHECK(n == 5 && !memcmp(buf, "world", 5), "%s: pread at 6 gave %ld '%.5s'", what, n, buf);
    CHECK(pos(fd) == 3, "%s: position %ld after pread", what, pos(fd));
    n = sc(SYS_pwrite64, fd, (long)"J", 1, 0);
    CHECK(n == 1 && pos(fd) == 3, "%s: pwrite at 0 gave %ld, position %ld", what, n, pos(fd));
    n = sc(SYS_pwrite64, fd, (long)"!", 1, 11);
    CHECK(n == 1, "%s: pwrite past the end gave %ld", what, n);
    memset(buf, 0, sizeof buf);
    n = sc(SYS_pread64, fd, (long)buf, 16, 0);
    CHECK(n == 12 && !memcmp(buf, "Jello world!", 12), "%s: whole file %ld '%.12s'", what, n, buf);
    n = sc(SYS_pread64, fd, (long)buf, 4, 100);
    CHECK(n == 0, "%s: pread past the end gave %ld", what, n);
    // a plain read goes on from the position
    n = sc(SYS_read, fd, (long)buf, 2, 0);
    CHECK(n == 2 && !memcmp(buf, "lo", 2) && pos(fd) == 5, "%s: read after them gave %ld '%.2s' at %ld", what, n, buf, pos(fd));
    CHECK(sc(SYS_pread64, fd, (long)buf, 1, -1) == -EINVAL_, "%s: negative offset", what);
    // a dup shares the position: pread through it must not move it either
    long d = sc(SYS_dup, fd, 0, 0, 0);
    sc(SYS_pread64, d, (long)buf, 3, 0);
    CHECK(pos(fd) == 5, "%s: position %ld after pread on a dup", what, pos(fd));
    sc(SYS_close, d, 0, 0, 0);
}

static long make(long fd) {
    sc(SYS_write, fd, (long)"hello world", 11, 0);
    sc(SYS_lseek, fd, 3, SEEK_SET, 0);
    return fd;
}

int main(void) {
    long t = sc(SYS_openat, AT_FDCWD_, (long)"/tmp/pio_test", O_RDWR_ | O_CREAT_ | O_TRUNC_, 0644);
    on("/tmp (ramfs)", make(t));
    long m = sc(SYS_openat, AT_FDCWD_, (long)"/mnt/pio_test.tmp", O_RDWR_ | O_CREAT_ | O_TRUNC_, 0644);
    if (m >= 0) on("/mnt (ext2)", make(m)); else printf("/mnt not writable (%ld): skipped\n", m);
    long mf = sc(SYS_memfd_create, (long)"pio", 0, 0, 0);
    on("memfd", make(mf));

    printf("errors and rights:\n");
    int p[2];
    sc(SYS_pipe, (long)p, 0, 0, 0);
    char c;
    CHECK(sc(SYS_pread64, p[0], (long)&c, 1, 0) == -ESPIPE_, "pread on a pipe");
    CHECK(sc(SYS_pwrite64, p[1], (long)"x", 1, 0) == -ESPIPE_, "pwrite on a pipe");
    CHECK(sc(SYS_pread64, 99, (long)&c, 1, 0) == -EBADF_, "pread on a closed fd");
    CHECK(sc(SYS_pread64, mf, (long)&c, 0, 0) == 0, "zero bytes");
    long r = sc(SYS_dup, mf, 0, 0, 0);
    cap_rights_limit((int)r, CAP_READ);  // no CAP_SEEK
    CHECK(sc(SYS_pread64, r, (long)&c, 1, 0) == -ENOTCAPABLE, "pread needs CAP_SEEK too");
    long w = sc(SYS_dup, mf, 0, 0, 0);
    cap_rights_limit((int)w, CAP_WRITE | CAP_SEEK);
    CHECK(sc(SYS_pwrite64, w, (long)"Z", 1, 0) == 1, "pwrite with write+seek");
    CHECK(sc(SYS_pread64, w, (long)&c, 1, 0) == -ENOTCAPABLE, "pread needs CAP_READ");
    // in capability mode, through held descriptors
    long pid = sc(SYS_fork, 0, 0, 0, 0);
    if (pid == 0) {
        sc(407, 0, 0, 0, 0); // cap_enter
        char b[3] = {0};
        long a = sc(SYS_pread64, mf, (long)b, 2, 0);
        long z = sc(SYS_pwrite64, mf, (long)"Q", 1, 1);
        sc(SYS_exit, (a == 2 && b[0] == 'Z' && z == 1) ? 7 : 1, 0, 0, 0);
    }
    int st = 0;
    sc(SYS_wait4, pid, (long)&st, 0, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 7, "in capability mode: status %#x", st);
    sc(SYS_pread64, mf, (long)&c, 1, 1);
    CHECK(c == 'Q', "the child's pwrite landed: '%c'", c);
    sc(SYS_unlink, (long)"/tmp/pio_test", 0, 0, 0);
    if (m >= 0) sc(SYS_unlink, (long)"/mnt/pio_test.tmp", 0, 0, 0);
    printf("pio_test: %s (%d failures)\n", failures ? "FAIL" : "PASS", failures);
    return failures ? 1 : 0;
}
