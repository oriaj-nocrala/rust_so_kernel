// eventfd2(290) / eventfd(284): counter semantics, EFD_SEMAPHORE, EFD_NONBLOCK / blocking reads, EFD_CLOEXEC, readiness through
// epoll (the way mio's Waker uses it), errors — and the rest of what mio sets up (epoll_create1 + CLOEXEC, a non-blocking
// socketpair registered in the same epoll).
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <errno.h>
#include <unistd.h>
#include <fcntl.h>
#include <time.h>
#include <sys/socket.h>
#include <sys/wait.h>

#define printf(...) ((printf)(__VA_ARGS__), fflush(stdout))
static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

struct epoll_event { uint32_t events; uint64_t data; } __attribute__((packed));
#define EPOLLIN 1u
#define EPOLLRDHUP 0x2000u
#define EPOLLET 0x80000000u
static long sc(long nr, long a, long b, long c, long d) {
    long r;
    register long r10 __asm__("r10") = d;
    __asm__ volatile("syscall" : "=a"(r) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10) : "rcx", "r11", "memory");
    return r;
}
enum { EFD_SEMAPHORE = 1, EFD_NONBLOCK = 0x800, EFD_CLOEXEC = 0x80000 };
static long eventfd2(unsigned v, int flags) { return sc(290, v, flags, 0, 0); }
static long rd(int fd, uint64_t *v) { return read(fd, v, 8) == 8 ? 0 : -errno; }
static long wr(int fd, uint64_t v) { return write(fd, &v, 8) == 8 ? 0 : -errno; }
static int ready(int ep, struct epoll_event *ev) { return (int)sc(232, ep, (long)ev, 4, 0); }

int main(void) {
    uint64_t v;

    printf("counter semantics\n");
    int e = (int)eventfd2(5, EFD_NONBLOCK);
    CHECK(e >= 0, "eventfd2 gave %d", e);
    CHECK(rd(e, &v) == 0 && v == 5, "first read gave %llu, wanted the initial 5", (unsigned long long)v);
    CHECK(rd(e, &v) == -EAGAIN, "a read at zero gave %ld, wanted -EAGAIN", rd(e, &v));
    CHECK(wr(e, 3) == 0 && wr(e, 4) == 0, "writes");
    CHECK(rd(e, &v) == 0 && v == 7, "the read gave %llu, wanted the sum 7", (unsigned long long)v);
    CHECK(wr(e, ~0ull) == -EINVAL, "writing 0xffffffffffffffff gave %ld", wr(e, ~0ull));
    char small[4];
    CHECK(read(e, small, 4) == -1 && errno == EINVAL, "a 4-byte read gave errno %d", errno);
    CHECK(write(e, small, 4) == -1 && errno == EINVAL, "a 4-byte write gave errno %d", errno);
    close(e);

    printf("EFD_SEMAPHORE: each read takes 1\n");
    e = (int)eventfd2(3, EFD_NONBLOCK | EFD_SEMAPHORE);
    CHECK(rd(e, &v) == 0 && v == 1, "read gave %llu", (unsigned long long)v);
    CHECK(rd(e, &v) == 0 && v == 1 && rd(e, &v) == 0 && v == 1, "reads 2 and 3");
    CHECK(rd(e, &v) == -EAGAIN, "the 4th read gave %ld", rd(e, &v));
    close(e);

    printf("eventfd(284) with no flags, F_GETFD / F_GETFL\n");
    int old = (int)sc(284, 2, 0, 0, 0);
    CHECK(old >= 0 && rd(old, &v) == 0 && v == 2, "old eventfd: %d", old);
    close(old);
    int cx = (int)eventfd2(0, EFD_CLOEXEC | EFD_NONBLOCK);
    CHECK((fcntl(cx, F_GETFD) & FD_CLOEXEC) != 0, "EFD_CLOEXEC not applied");
    CHECK((fcntl(cx, F_GETFL) & O_NONBLOCK) != 0, "EFD_NONBLOCK not reported by F_GETFL");
    CHECK(eventfd2(0, 0x1000000) == -EINVAL, "an unknown flag");
    close(cx);

    printf("a blocking read waits for a write\n");
    e = (int)eventfd2(0, 0);
    pid_t c = fork();
    if (c == 0) { struct timespec ts = {0, 150000000}; nanosleep(&ts, NULL); wr(e, 9); _exit(0); }
    struct timespec t0, t1; clock_gettime(CLOCK_MONOTONIC, &t0);
    long r = rd(e, &v);
    clock_gettime(CLOCK_MONOTONIC, &t1);
    double el = (t1.tv_sec - t0.tv_sec) + (t1.tv_nsec - t0.tv_nsec) / 1e9;
    CHECK(r == 0 && v == 9, "read gave r=%ld v=%llu", r, (unsigned long long)v);
    CHECK(el >= 0.12 && el < 1.0, "the read waited %.3f s, wanted about 0.15", el);
    waitpid(c, NULL, 0);
    close(e);

    printf("readiness through epoll, as mio uses it\n");
    int ep = (int)sc(291, 0x80000, 0, 0, 0);
    CHECK(ep >= 0, "epoll_create1(EPOLL_CLOEXEC) gave %d", ep);
    e = (int)eventfd2(0, EFD_CLOEXEC | EFD_NONBLOCK);
    struct epoll_event ev = { .events = EPOLLIN | EPOLLRDHUP | EPOLLET, .data = 77 }, out[4];
    CHECK(sc(233, ep, 1, e, (long)&ev) == 0, "epoll_ctl ADD of the eventfd");
    int sv[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0, sv) == 0, "socketpair errno %d", errno);
    ev.data = 88;
    CHECK(sc(233, ep, 1, sv[0], (long)&ev) == 0, "epoll_ctl ADD of the socketpair end");
    CHECK(ready(ep, out) == 0, "nothing should be ready yet");
    wr(e, 1);
    int n = ready(ep, out);
    CHECK(n == 1 && out[0].data == 77 && (out[0].events & EPOLLIN), "after a write: n=%d data=%llu", n, (unsigned long long)out[0].data);
    rd(e, &v);
    CHECK(ready(ep, out) == 0, "after the read the eventfd is not ready again");
    // a blocked epoll_wait is woken by a write from another process
    c = fork();
    if (c == 0) { struct timespec ts = {0, 150000000}; nanosleep(&ts, NULL); wr(e, 1); _exit(0); }
    clock_gettime(CLOCK_MONOTONIC, &t0);
    n = (int)sc(232, ep, (long)out, 4, 3000);
    clock_gettime(CLOCK_MONOTONIC, &t1);
    el = (t1.tv_sec - t0.tv_sec) + (t1.tv_nsec - t0.tv_nsec) / 1e9;
    CHECK(n == 1 && out[0].data == 77, "blocked epoll_wait: n=%d", n);
    CHECK(el >= 0.12 && el < 1.5, "woke after %.3f s, wanted about 0.15", el);
    waitpid(c, NULL, 0);
    close(sv[0]); close(sv[1]);

    printf(failures ? "eventfd_test: FAIL\n" : "eventfd_test: PASS\n");
    return failures != 0;
}
