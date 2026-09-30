// epoll beyond 16: 100 watches in one instance, 40 instances, maxevents far past 16 (both when events are ready at once and when the
// wait blocks and a wake delivers them), EEXIST / ENOENT / re-adding after DEL, and more ready fds than maxevents.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <errno.h>
#include <unistd.h>
#include <fcntl.h>
#include <time.h>
#include <sys/wait.h>

// mlibc's sysroot has no <sys/epoll.h>: the raw syscalls and Linux's packed `struct epoll_event`.
struct epoll_event { uint32_t events; uint64_t data_u64; } __attribute__((packed));
#define EPOLLIN 1u
#define EPOLL_CTL_ADD 1
#define EPOLL_CTL_DEL 2
#define EPOLL_CTL_MOD 3
static long sc(long nr, long a, long b, long c, long d) {
    long r;
    register long r10 __asm__("r10") = d;
    __asm__ volatile("syscall" : "=a"(r) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10) : "rcx", "r11", "memory");
    return r;
}
// libc-style wrappers: -1 with errno on failure
static int wrap(long r) { if (r < 0) { errno = (int)-r; return -1; } return (int)r; }
static int epoll_create1(int flags) { return wrap(sc(291, flags, 0, 0, 0)); }
static int epoll_ctl(int ep, int op, int fd, struct epoll_event *ev) { return wrap(sc(233, ep, op, fd, (long)ev)); }
static int epoll_wait(int ep, struct epoll_event *evs, int max, int timeout) { return wrap(sc(232, ep, (long)evs, max, timeout)); }

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

#define N 100
static int rd[N], wr[N];

static int add(int ep, int i) {
    struct epoll_event ev = { .events = EPOLLIN, .data_u64 = 1000 + i };
    return epoll_ctl(ep, EPOLL_CTL_ADD, rd[i], &ev);
}

int main(void) {
    for (int i = 0; i < N; i++) {
        int p[2];
        if (pipe(p) != 0) { printf("pipe %d failed: %d\n", i, errno); return 1; }
        rd[i] = p[0]; wr[i] = p[1];
    }
    struct epoll_event *evs = malloc(1000 * sizeof *evs);

    printf("%d watches in one instance\n", N);
    int ep = epoll_create1(0);
    int added = 0, first_err = 0;
    for (int i = 0; i < N; i++) {
        if (add(ep, i) == 0) added++; else if (!first_err) first_err = errno;
    }
    CHECK(added == N, "added %d of %d (first errno %d)", added, N, first_err);

    printf("maxevents past 16, all ready at once\n");
    write(wr[0], "x", 1); write(wr[50], "x", 1); write(wr[99], "x", 1);
    int n = epoll_wait(ep, evs, 64, 0);
    CHECK(n == 3, "epoll_wait(maxevents 64) returned %d (errno %d)", n, errno);
    CHECK(n == 3 && evs[0].data_u64 == 1000 && evs[1].data_u64 == 1050 && evs[2].data_u64 == 1099, "events %llu %llu %llu",
          (unsigned long long)evs[0].data_u64, (unsigned long long)evs[1].data_u64, (unsigned long long)evs[2].data_u64);
    char c;
    read(rd[0], &c, 1); read(rd[50], &c, 1); read(rd[99], &c, 1);

    printf("more ready than maxevents\n");
    for (int i = 0; i < N; i++) write(wr[i], "x", 1);
    n = epoll_wait(ep, evs, 10, 0);
    CHECK(n == 10, "returned %d, wanted maxevents 10", n);
    n = epoll_wait(ep, evs, 1000, 0);
    CHECK(n == N, "maxevents 1000 returned %d, wanted %d", n, N);
    for (int i = 0; i < N; i++) read(rd[i], &c, 1);

    printf("a blocked wait with a large maxevents is woken with its event\n");
    pid_t child = fork();
    if (child == 0) {
        struct timespec ts = {0, 100000000};
        nanosleep(&ts, NULL);
        write(wr[77], "y", 1);
        _exit(0);
    }
    n = epoll_wait(ep, evs, 1000, 3000);
    CHECK(n == 1, "returned %d (errno %d)", n, errno);
    CHECK(n == 1 && evs[0].data_u64 == 1077 && (evs[0].events & EPOLLIN), "data %llu events %x", (unsigned long long)evs[0].data_u64, evs[0].events);
    waitpid(child, NULL, 0);
    read(rd[77], &c, 1);

    printf("EEXIST, ENOENT, DEL then ADD\n");
    CHECK(add(ep, 5) == -1 && errno == EEXIST, "duplicate add gave errno %d", errno);
    CHECK(epoll_ctl(ep, EPOLL_CTL_DEL, rd[5], NULL) == 0, "del");
    CHECK(epoll_ctl(ep, EPOLL_CTL_DEL, rd[5], NULL) == -1 && errno == ENOENT, "second del gave errno %d", errno);
    struct epoll_event ev = { .events = EPOLLIN, .data_u64 = 1 };
    CHECK(epoll_ctl(ep, EPOLL_CTL_MOD, rd[5], &ev) == -1 && errno == ENOENT, "mod of a removed watch gave errno %d", errno);
    CHECK(add(ep, 5) == 0, "re-add after del");
    write(wr[5], "z", 1);
    n = epoll_wait(ep, evs, 8, 0);
    CHECK(n == 1 && evs[0].data_u64 == 1005, "re-added watch reports: n=%d", n);
    read(rd[5], &c, 1);

    printf("40 epoll instances at once\n");
    int eps[40], made = 0;
    for (int i = 0; i < 40; i++) {
        eps[i] = epoll_create1(0);
        if (eps[i] >= 0) made++;
    }
    CHECK(made == 40, "created %d of 40 (errno %d)", made, errno);
    // each one is independent: a watch in the last one only shows up there
    ev.events = EPOLLIN; ev.data_u64 = 42;
    CHECK(made == 40 && epoll_ctl(eps[39], EPOLL_CTL_ADD, rd[9], &ev) == 0, "add to the last instance");
    write(wr[9], "w", 1);
    CHECK(made == 40 && epoll_wait(eps[39], evs, 4, 0) == 1 && evs[0].data_u64 == 42, "the last instance sees its watch");
    CHECK(made == 40 && epoll_wait(eps[0], evs, 4, 0) == 0, "an empty instance sees nothing");

    printf("epoll_create1 flags\n");
    int cx = epoll_create1(0x80000);                       // EPOLL_CLOEXEC
    CHECK(cx >= 0 && (fcntl(cx, F_GETFD) & FD_CLOEXEC), "EPOLL_CLOEXEC did not set FD_CLOEXEC");
    int nc = epoll_create1(0);
    CHECK(nc >= 0 && !(fcntl(nc, F_GETFD) & FD_CLOEXEC), "a plain epoll fd is close-on-exec");
    CHECK(epoll_create1(0x1) == -1 && errno == EINVAL, "an unknown flag gave errno %d", errno);
    CHECK(wrap(sc(213, 0, 0, 0, 0)) == -1 && errno == EINVAL, "epoll_create(0) gave errno %d", errno);

    printf(failures ? "epoll_scale_test: FAIL\n" : "epoll_scale_test: PASS\n");
    return failures != 0;
}
