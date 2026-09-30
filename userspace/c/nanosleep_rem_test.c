// nanosleep(35) and clock_nanosleep(230) are Linux's, and a sleep a handler interrupts reports the time it had left in `rem`
// (relative sleeps only; a sleep that completes, or an absolute one, leaves `rem` alone). Also through mlibc: nanosleep()/sleep().
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <signal.h>
#include <time.h>
#include <sys/wait.h>

static int failures;
#define printf(...) ((printf)(__VA_ARGS__), fflush(stdout))
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

static long sc(long nr, long a, long b, long c, long d) {
    long r;
    register long r10 __asm__("r10") = d;
    __asm__ volatile("syscall" : "=a"(r) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10) : "rcx", "r11", "memory");
    return r;
}
enum { NANOSLEEP = 35, CLOCK_NANOSLEEP = 230, EINTR_ = 4 };

static void handler(int sig) { (void)sig; }

// A child that signals the parent after 100 ms, then waits to be reaped.
static pid_t poke_soon(void) {
    pid_t c = fork();
    if (c == 0) {
        struct timespec ts = {0, 100000000};
        nanosleep(&ts, NULL);
        kill(getppid(), SIGUSR1);
        _exit(0);
    }
    return c;
}

static long ns_of(struct timespec t) { return t.tv_sec * 1000000000L + t.tv_nsec; }
#define SENTINEL 0x5a5a5a5a

static void expect_left(const char *what, long r, struct timespec rem, long asked_ns) {
    long left = ns_of(rem);
    CHECK(r == -EINTR_, "%s returned %ld, wanted -EINTR", what, r);
    CHECK(left > asked_ns / 2 && left < asked_ns - 50000000L, "%s: rem is %ld ns of %ld asked (slept about 100 ms)", what, left, asked_ns);
}

int main(void) {
    struct sigaction sa = {0};
    sa.sa_handler = handler;                              // no SA_RESTART: the sleep ends with EINTR
    sigaction(SIGUSR1, &sa, NULL);

    printf("nanosleep(35), Linux ABI\n");
    struct timespec req = {0, 50000000}, rem = {SENTINEL, SENTINEL};
    struct timespec t0, t1;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    CHECK(sc(NANOSLEEP, (long)&req, (long)&rem, 0, 0) == 0, "a complete sleep returns 0");
    clock_gettime(CLOCK_MONOTONIC, &t1);
    CHECK(ns_of(t1) - ns_of(t0) >= 49000000L, "slept only %ld ns", ns_of(t1) - ns_of(t0));
    CHECK(rem.tv_sec == SENTINEL && rem.tv_nsec == SENTINEL, "a complete sleep must not write rem");
    struct timespec bad = {0, 1000000000};
    CHECK(sc(NANOSLEEP, (long)&bad, 0, 0, 0) == -22, "tv_nsec = 1e9 is EINVAL");

    printf("nanosleep interrupted by a handler\n");
    pid_t c = poke_soon();
    req = (struct timespec){2, 0};
    rem = (struct timespec){SENTINEL, SENTINEL};
    long r = sc(NANOSLEEP, (long)&req, (long)&rem, 0, 0);
    expect_left("nanosleep", r, rem, 2000000000L);
    waitpid(c, NULL, 0);

    printf("clock_nanosleep relative\n");
    c = poke_soon();
    rem = (struct timespec){SENTINEL, SENTINEL};
    r = sc(CLOCK_NANOSLEEP, CLOCK_MONOTONIC, 0, (long)&req, (long)&rem);
    expect_left("clock_nanosleep", r, rem, 2000000000L);
    waitpid(c, NULL, 0);

    printf("rem may be NULL\n");
    c = poke_soon();
    r = sc(NANOSLEEP, (long)&req, 0, 0, 0);
    CHECK(r == -EINTR_, "returned %ld", r);
    waitpid(c, NULL, 0);

    printf("clock_nanosleep absolute never writes rem\n");
    c = poke_soon();
    clock_gettime(CLOCK_MONOTONIC, &t0);
    struct timespec until = {t0.tv_sec + 2, t0.tv_nsec};
    rem = (struct timespec){SENTINEL, SENTINEL};
    r = sc(CLOCK_NANOSLEEP, CLOCK_MONOTONIC, 1, (long)&until, (long)&rem);
    CHECK(r == -EINTR_, "returned %ld", r);
    CHECK(rem.tv_sec == SENTINEL && rem.tv_nsec == SENTINEL, "an absolute sleep wrote rem");
    waitpid(c, NULL, 0);

    printf("through mlibc\n");
    c = poke_soon();
    rem = (struct timespec){SENTINEL, SENTINEL};
    int lr = nanosleep(&req, &rem);
    CHECK(lr == -1, "nanosleep() returned %d", lr);
    CHECK(ns_of(rem) > 1000000000L && ns_of(rem) < 1950000000L, "mlibc rem %ld ns", ns_of(rem));
    waitpid(c, NULL, 0);
    c = poke_soon();
    unsigned left = sleep(3);
    CHECK(left == 2 || left == 3, "sleep(3) interrupted after 100 ms returned %u", left);
    waitpid(c, NULL, 0);

    printf(failures ? "nanosleep_rem_test: FAIL\n" : "nanosleep_rem_test: PASS\n");
    return failures != 0;
}
