// poll()/epoll on pipes reports real readiness and wakes sleepers: POLLIN when data arrives, POLLHUP when the writers are gone,
// POLLOUT when a full pipe is drained, POLLERR when the readers are gone. (Pipes used to be "always ready", so a poll loop spun.)
#define _GNU_SOURCE
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>
#include <poll.h>
#include <errno.h>
#include <time.h>
#include <pthread.h>

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
enum { SYS_pipe2 = 293, O_NONBLOCK_ = 0x800, SYS_epoll_create = 213, SYS_epoll_wait = 232, SYS_epoll_ctl = 233 };
enum { EPOLLIN_ = 1, EPOLL_CTL_ADD_ = 1 };
// mlibc's sysroot has no <sys/epoll.h> here: Linux's packed struct, raw syscalls
struct epoll_ev { uint32_t events; uint64_t data; } __attribute__((packed));

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

static int64_t now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static int wfd, rfd;
static void *delayed_write(void *arg) { (void)arg; usleep(80000); write(wfd, "x", 1); return NULL; }
static void *delayed_drain(void *arg) { (void)arg; usleep(80000); char buf[8192]; read(rfd, buf, sizeof buf); return NULL; }
static void *delayed_close_w(void *arg) { (void)arg; usleep(80000); close(wfd); return NULL; }

static void test_read_side(void) {
    printf("read end\n");
    int p[2];
    sc(SYS_pipe2, (long)p, 0, 0);
    rfd = p[0]; wfd = p[1];
    struct pollfd pf = { .fd = rfd, .events = POLLIN };
    int64_t t0 = now_ms();
    int r = poll(&pf, 1, 100);
    int64_t dt = now_ms() - t0;
    CHECK(r == 0 && pf.revents == 0, "empty pipe: poll returned %d revents %#x", r, pf.revents);
    CHECK(dt >= 90, "and it waited (%ld ms)", (long)dt);

    pthread_t t;
    pthread_create(&t, NULL, delayed_write, NULL);
    t0 = now_ms();
    r = poll(&pf, 1, 2000);
    dt = now_ms() - t0;
    pthread_join(t, NULL);
    CHECK(r == 1 && (pf.revents & POLLIN), "a write woke the poller: %d revents %#x", r, pf.revents);
    CHECK(dt >= 50 && dt < 1000, "after the write, not before, not at the timeout (%ld ms)", (long)dt);
    char c;
    CHECK(read(rfd, &c, 1) == 1 && c == 'x', "data is there");
    pf.revents = 0;
    CHECK(poll(&pf, 1, 0) == 0, "drained: not ready again");

    pthread_create(&t, NULL, delayed_close_w, NULL);
    t0 = now_ms();
    r = poll(&pf, 1, 2000);
    dt = now_ms() - t0;
    pthread_join(t, NULL);
    CHECK(r == 1 && (pf.revents & POLLHUP), "closing the writer woke the poller with POLLHUP: %d revents %#x", r, pf.revents);
    CHECK(dt < 1000, "promptly (%ld ms)", (long)dt);
    close(rfd);
}

static void test_write_side(void) {
    printf("write end\n");
    int p[2];
    sc(SYS_pipe2, (long)p, O_NONBLOCK_, 0);
    rfd = p[0]; wfd = p[1];
    struct pollfd pf = { .fd = wfd, .events = POLLOUT };
    CHECK(poll(&pf, 1, 0) == 1 && (pf.revents & POLLOUT), "an empty pipe is writable");
    char buf[1024];
    memset(buf, 'a', sizeof buf);
    long total = 0;
    for (;;) {
        ssize_t n = write(wfd, buf, sizeof buf);
        if (n < 0) { CHECK(errno == EAGAIN, "a full non-blocking pipe says EAGAIN (errno %d)", errno); break; }
        total += n;
        if (total > (1 << 20)) { CHECK(0, "the pipe never filled"); break; }
    }
    pf.revents = 0;
    int64_t t0 = now_ms();
    int r = poll(&pf, 1, 100);
    CHECK(r == 0 && !(pf.revents & POLLOUT), "full pipe: not writable (%d, %#x)", r, pf.revents);
    CHECK(now_ms() - t0 >= 90, "and poll waited");

    pthread_t t;
    pthread_create(&t, NULL, delayed_drain, NULL);
    t0 = now_ms();
    r = poll(&pf, 1, 2000);
    int64_t dt = now_ms() - t0;
    pthread_join(t, NULL);
    CHECK(r == 1 && (pf.revents & POLLOUT), "a read woke the writer's poll: %d revents %#x", r, pf.revents);
    CHECK(dt >= 50 && dt < 1000, "after the read (%ld ms)", (long)dt);

    close(rfd);
    pf.revents = 0;
    CHECK(poll(&pf, 1, 0) == 1 && (pf.revents & POLLERR), "no reader left: POLLERR (%#x)", pf.revents);
    close(wfd);
}

static void test_epoll(void) {
    printf("epoll\n");
    int p[2];
    sc(SYS_pipe2, (long)p, 0, 0);
    rfd = p[0]; wfd = p[1];
    int ep = (int)sc(SYS_epoll_create, 1, 0, 0);
    struct epoll_ev ev = { .events = EPOLLIN_, .data = (uint64_t)rfd };
    CHECK(sc4(SYS_epoll_ctl, ep, EPOLL_CTL_ADD_, rfd, (long)&ev) == 0, "add");
    struct epoll_ev out[2];
    int64_t t0 = now_ms();
    long n = sc4(SYS_epoll_wait, ep, (long)out, 2, 100);
    CHECK(n == 0 && now_ms() - t0 >= 90, "empty: nothing, and it waited (%ld)", n);
    pthread_t t;
    pthread_create(&t, NULL, delayed_write, NULL);
    t0 = now_ms();
    n = sc4(SYS_epoll_wait, ep, (long)out, 2, 2000);
    int64_t dt = now_ms() - t0;
    pthread_join(t, NULL);
    CHECK(n == 1 && (out[0].events & EPOLLIN_) && dt < 1000, "a write wakes epoll_wait (%ld, %ld ms)", n, (long)dt);
    close(ep);
    close(rfd);
    close(wfd);
}

int main(void) {
    test_read_side();
    test_write_side();
    test_epoll();
    if (failures) {
        printf("pipe_poll_test: %d FAILED\n", failures);
        return 1;
    }
    printf("pipe_poll_test: OK\n");
    return 0;
}
