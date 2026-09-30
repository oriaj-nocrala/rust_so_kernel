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
#include <signal.h>
#include <sys/wait.h>

// mlibc's sysroot has no <sys/epoll.h>: the raw syscalls and Linux's packed `struct epoll_event`.
struct epoll_event { uint32_t events; uint64_t data_u64; } __attribute__((packed));
#define EPOLLIN 1u
#define EPOLLONESHOT 0x40000000u
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

static long sc6(long nr, long a, long b, long c, long d, long e, long f) {
    long r;
    register long r10 __asm__("r10") = d;
    register long r8 __asm__("r8") = e;
    register long r9 __asm__("r9") = f;
    __asm__ volatile("syscall" : "=a"(r) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8), "r"(r9) : "rcx", "r11", "memory");
    return r;
}
static volatile int usr1_calls;
static void usr1(int sig) { (void)sig; usr1_calls++; }

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

    printf("an epoll fd can be duplicated (tokio's Registry::try_clone)\n");
    int dp = epoll_create1(0);
    int d1 = dup(dp);
    int d2 = (int)sc(292, dp, 100, 0x80000, 0);                 // dup3(dp, 100, O_CLOEXEC)
    int d3 = fcntl(dp, 1030, 50);                               // F_DUPFD_CLOEXEC, at least fd 50
    CHECK(d1 >= 0 && d2 == 100 && d3 >= 50, "dup gave %d, dup3 %d, F_DUPFD_CLOEXEC %d (errno %d)", d1, d2, d3, errno);
    ev.events = EPOLLIN; ev.data_u64 = 55;
    CHECK(epoll_ctl(d1, EPOLL_CTL_ADD, rd[40], &ev) == 0, "ctl through the dup");
    write(wr[40], "d", 1);
    CHECK(epoll_wait(dp, evs, 4, 0) == 1 && evs[0].data_u64 == 55, "wait on the original sees a watch added through the dup");
    CHECK(epoll_wait(d3, evs, 4, 0) == 1, "wait on another dup sees it too");
    close(dp); close(d1);
    CHECK(epoll_wait(d2, evs, 4, 0) == 1 && evs[0].data_u64 == 55, "the instance outlives the closed fds while one dup is left");
    close(d2); close(d3);
    int reuse = open("/dev/null", O_RDONLY);                    // takes the lowest free fd, a number an epoll fd just had
    CHECK(epoll_wait(reuse, evs, 4, 0) == -1 && errno == EBADF, "a reused fd number is not an epoll fd (errno %d)", errno);
    close(reuse);
    read(rd[40], &c, 1);

    printf("EPOLLONESHOT: one report, then silent until MOD\n");
    int os = epoll_create1(0);
    ev.events = EPOLLIN | EPOLLONESHOT; ev.data_u64 = 7;
    CHECK(epoll_ctl(os, EPOLL_CTL_ADD, rd[20], &ev) == 0, "add oneshot");
    write(wr[20], "o", 1);
    CHECK(epoll_wait(os, evs, 4, 0) == 1 && evs[0].data_u64 == 7, "first report");
    CHECK(epoll_wait(os, evs, 4, 0) == 0, "a oneshot watch reported twice (the fd is still readable)");
    CHECK(epoll_ctl(os, EPOLL_CTL_MOD, rd[20], &ev) == 0, "mod re-arms");
    CHECK(epoll_wait(os, evs, 4, 0) == 1 && evs[0].data_u64 == 7, "reports again after MOD");
    CHECK(epoll_wait(os, evs, 4, 0) == 0, "and is silent again");
    read(rd[20], &c, 1);

    printf("epoll_pwait: the mask is in force during the call only\n");
    struct sigaction sa = {0};
    sa.sa_handler = usr1;
    sigaction(SIGUSR1, &sa, NULL);
    int pw = epoll_create1(0);
    ev.events = EPOLLIN; ev.data_u64 = 31;
    epoll_ctl(pw, EPOLL_CTL_ADD, rd[31], &ev);
    sigset_t usr1set, empty, cur;
    sigemptyset(&usr1set); sigaddset(&usr1set, SIGUSR1);
    sigemptyset(&empty);
    uint64_t none = 0, only_usr1 = 1ull << (SIGUSR1 - 1);   // kernel sigset: bit N-1 is signal N

    // (1) SIGUSR1 blocked by the caller, unblocked by pwait's mask: it interrupts the wait, and the old mask comes back
    sigprocmask(SIG_BLOCK, &usr1set, NULL);
    usr1_calls = 0;
    child = fork();
    if (child == 0) { struct timespec ts = {0, 100000000}; nanosleep(&ts, NULL); kill(getppid(), SIGUSR1); _exit(0); }
    long r = sc6(281, pw, (long)evs, 4, 2000, (long)&none, 8);
    CHECK(r == -4, "pwait with an empty mask returned %ld, wanted -EINTR", r);
    CHECK(usr1_calls == 1, "the handler ran %d times", usr1_calls);
    sigprocmask(SIG_BLOCK, NULL, &cur);
    CHECK(sigismember(&cur, SIGUSR1) == 1, "the caller's mask (SIGUSR1 blocked) was not restored");
    waitpid(child, NULL, 0);
    sigprocmask(SIG_SETMASK, &empty, NULL);

    // (2) SIGUSR1 unblocked by the caller, blocked by pwait's mask: the wait ends with the event, and the signal is
    //     delivered only when the old mask is back (as the call returns)
    usr1_calls = 0;
    child = fork();
    if (child == 0) {
        struct timespec a = {0, 100000000}, b = {0, 200000000};
        nanosleep(&a, NULL); kill(getppid(), SIGUSR1);
        nanosleep(&b, NULL); write(wr[31], "e", 1);
        _exit(0);
    }
    r = sc6(281, pw, (long)evs, 4, 3000, (long)&only_usr1, 8);
    CHECK(r == 1 && evs[0].data_u64 == 31, "pwait with SIGUSR1 masked returned %ld, wanted the event", r);
    CHECK(usr1_calls == 1, "the signal held back during the call ran %d times after it", usr1_calls);
    waitpid(child, NULL, 0);
    read(rd[31], &c, 1);
    CHECK(sc6(281, pw, (long)evs, 4, 0, 0, 8) == 0, "a NULL mask is a plain epoll_wait");
    CHECK(sc6(281, pw, (long)evs, 4, 0, (long)&none, 4) == -22, "a sigsetsize other than 8 is EINVAL");

    printf(failures ? "epoll_scale_test: FAIL\n" : "epoll_scale_test: PASS\n");
    return failures != 0;
}
