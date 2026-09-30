// The descriptor table has 256 slots (it had 16): open past the old limit, dup to the top, EMFILE when full, fork and exec carry the
// high fds, and poll/epoll/lseek work on them. EBADF for a number past the table.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <poll.h>
#include <errno.h>
#include <sys/wait.h>

static long sc(long nr, long a, long b, long c) {
    long ret;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(nr), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return ret;
}
static long sc4(long nr, long a, long b, long c, long d) {
    long ret;
    register long r10 __asm__("r10") = d;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10) : "rcx", "r11", "memory");
    return ret;
}
enum { SYS_pipe2 = 293, SYS_dup3 = 292, SYS_epoll_create = 213, SYS_epoll_wait = 232, SYS_epoll_ctl = 233, O_CLOEXEC_ = 0x80000 };
struct epoll_ev { uint32_t events; uint64_t data; } __attribute__((packed));

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "child")) {
        // exec'd: fd 200 (plain) is open and readable as a pipe end, fd 201 (close-on-exec) is closed; report through fd 3
        int ok = fcntl(200, F_GETFD) >= 0 && fcntl(201, F_GETFD) < 0;
        write(3, ok ? "Y" : "N", 1);
        return 0;
    }

    printf("fd table\n");
    int fds[300], n = 0;
    for (;;) {
        int fd = open("/dev/null", O_RDONLY);
        if (fd < 0) { CHECK(errno == EMFILE, "a full table says EMFILE (errno %d)", errno); break; }
        fds[n++] = fd;
        if (n >= 300) { CHECK(0, "no limit reached"); break; }
    }
    CHECK(n >= 250 && n <= 253, "opened %d descriptors (stdio takes 3 of 256)", n);
    CHECK(fds[n - 1] == 255, "the last one is fd %d", fds[n - 1]);
    for (int i = 0; i < n; i++) close(fds[i]);

    CHECK(sc(SYS_dup3, 1, 256, 0) < 0, "dup3 to 256 fails");
    CHECK(dup2(1, 255) == 255, "dup2 to 255");
    close(255);
    CHECK(fcntl(1, F_DUPFD, 100) == 100, "F_DUPFD from 100 gives 100");
    CHECK(lseek(100, 0, SEEK_CUR) != -2, "lseek on a high fd is not EBADF (%d)", errno);
    CHECK(fcntl(300, F_GETFD) == -1 && errno == EBADF, "fd 300: EBADF");
    close(100);

    printf("poll and epoll on high fds\n");
    int p[2];
    sc(SYS_pipe2, (long)p, 0, 0);
    CHECK(dup2(p[0], 150) == 150 && dup2(p[1], 151) == 151, "pipe ends at 150 and 151");
    close(p[0]); close(p[1]);
    struct pollfd pf[40];
    for (int i = 0; i < 40; i++) { pf[i].fd = 200 + i > 255 ? -1 : -1; pf[i].events = 0; pf[i].revents = 0; }
    pf[35].fd = 150; pf[35].events = POLLIN;         // an array of 40 entries, the pipe near its end
    CHECK(poll(pf, 40, 20) == 0, "40 entries, nothing ready");
    write(151, "z", 1);
    CHECK(poll(pf, 40, 1000) == 1 && (pf[35].revents & POLLIN), "fd 150 readable, seen through entry 35");
    char c;
    read(150, &c, 1);
    int ep = (int)sc(SYS_epoll_create, 1, 0, 0);
    CHECK(ep > 0, "epoll fd %d", ep);
    struct epoll_ev ev = { .events = 1, .data = 150 };
    CHECK(sc4(SYS_epoll_ctl, ep, 1, 150, (long)&ev) == 0, "epoll_ctl on fd 150");
    struct epoll_ev out[2];
    CHECK(sc4(SYS_epoll_wait, ep, (long)out, 2, 20) == 0, "not ready");
    write(151, "z", 1);
    CHECK(sc4(SYS_epoll_wait, ep, (long)out, 2, 1000) == 1 && out[0].data == 150, "epoll sees fd 150");
    close(ep);

    printf("fork and exec\n");
    int q[2];
    sc(SYS_pipe2, (long)q, 0, 0);                    // q: report pipe (plain)
    dup2(q[0], 3 + 250); close(q[0]);                // read end at 253
    dup2(q[1], 3); close(q[1]);                      // write end at fd 3, what the exec'd child writes to
    dup2(150, 200);
    sc(SYS_dup3, 150, 201, O_CLOEXEC_);
    pid_t pid = fork();
    if (pid == 0) {
        int seen = fcntl(200, F_GETFD) >= 0 && fcntl(201, F_GETFD) >= 0 && fcntl(253, F_GETFD) >= 0;
        if (!seen) _exit(9);
        execl("/mnt/bin/fdlimit_test", "fdlimit_test", "child", (char *)0);
        _exit(99);
    }
    char r = 0;
    read(253, &r, 1);
    int st = 0;
    waitpid(pid, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "child status %#x", st);
    CHECK(r == 'Y', "after exec fd 200 is open and fd 201 (close-on-exec) is closed: %c", r ? r : '?');

    if (failures) {
        printf("fdlimit_test: %d FAILED\n", failures);
        return 1;
    }
    printf("fdlimit_test: OK\n");
    return 0;
}
