// Linux-ABI syscalls that Rust's `std` (musl) and other runtimes expect: getrandom, /dev/urandom, gettid, tkill/tgkill,
// kill(pid, 0), sigaltstack, clock_nanosleep (relative and absolute), futex with timeouts, bitsets and requeue.
// Raw `syscall` instructions on purpose: this checks the kernel's ABI, not mlibc's wrappers. See docs/reference/syscalls.md.
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <signal.h>
#include <time.h>
#include <pthread.h>

static long sc(long nr, long a, long b, long c, long d, long e, long f) {
    long ret;
    register long r10 __asm__("r10") = d;
    register long r8 __asm__("r8") = e;
    register long r9 __asm__("r9") = f;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8), "r"(r9) : "rcx", "r11", "memory");
    return ret;
}

enum { SYS_sigaltstack = 131, SYS_gettid = 186, SYS_tkill = 200, SYS_futex = 202, SYS_clock_nanosleep = 230, SYS_tgkill = 234,
       SYS_getrandom = 318, SYS_kill = 62, SYS_getpid = 39 };
enum { EPERM_ = 1, ESRCH_ = 3, EAGAIN_ = 11, EINVAL_ = 22, ETIMEDOUT_ = 110, ENOSYS_ = 38 };
enum { FUTEX_WAIT = 0, FUTEX_WAKE = 1, FUTEX_REQUEUE = 3, FUTEX_CMP_REQUEUE = 4, FUTEX_WAIT_BITSET = 9, FUTEX_WAKE_BITSET = 10,
       FUTEX_PRIVATE = 128 };

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

static int64_t mono_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

static struct timespec ts_ns(int64_t ns) {
    struct timespec ts = { .tv_sec = ns / 1000000000, .tv_nsec = ns % 1000000000 };
    return ts;
}

static void test_getrandom(void) {
    printf("getrandom\n");
    unsigned char a[64], b[64], zero[64] = {0};
    memset(a, 0xaa, sizeof a);
    long r = sc(SYS_getrandom, (long)a, sizeof a, 0, 0, 0, 0);
    CHECK(r == 64, "returned %ld", r);
    CHECK(memcmp(a, zero, sizeof a) != 0, "all zero");
    sc(SYS_getrandom, (long)b, sizeof b, 0, 0, 0, 0);
    CHECK(memcmp(a, b, sizeof a) != 0, "two draws are equal");
    CHECK(sc(SYS_getrandom, (long)a, 0, 0, 0, 0, 0) == 0, "length 0");
    CHECK(sc(SYS_getrandom, (long)a, 8, 0x8, 0, 0, 0) == -EINVAL_, "unknown flag accepted");
    CHECK(sc(SYS_getrandom, (long)a, 8, 2 | 4, 0, 0, 0) == -EINVAL_, "GRND_RANDOM|GRND_INSECURE accepted");
    CHECK(sc(SYS_getrandom, (long)a, 8, 1, 0, 0, 0) == 8, "GRND_NONBLOCK");
    // a length that is not a multiple of anything, filled exactly and not one byte past
    unsigned char big[5003];
    memset(big, 0x55, sizeof big);
    CHECK(sc(SYS_getrandom, (long)(big + 1), 5000, 0, 0, 0, 0) == 5000, "5000 bytes");
    CHECK(big[0] == 0x55 && big[5001] == 0x55 && big[5002] == 0x55, "wrote outside its buffer");
    int ones = 0;
    for (int i = 1; i <= 5000; i++) ones += __builtin_popcount(big[i]);
    CHECK(ones > 19000 && ones < 21000, "%d one bits of 40000", ones);
    CHECK(sc(SYS_getrandom, 0, 8, 0, 0, 0, 0) < 0, "a null buffer accepted");
    // /dev/urandom and /dev/random
    unsigned char u1[32], u2[32];
    int fd = open("/dev/urandom", O_RDONLY);
    CHECK(fd >= 0, "open /dev/urandom");
    CHECK(read(fd, u1, sizeof u1) == 32 && read(fd, u2, sizeof u2) == 32, "read");
    CHECK(memcmp(u1, u2, 32) != 0, "urandom draws equal");
    CHECK(write(fd, "seed", 4) == 4, "write to urandom");
    close(fd);
    fd = open("/dev/random", O_RDONLY);
    CHECK(fd >= 0 && read(fd, u1, sizeof u1) == 32, "/dev/random");
    close(fd);
}

static volatile int got_usr1;
static void on_usr1(int sig) { (void)sig; got_usr1++; }

static void test_tid_and_signals(void) {
    printf("gettid, tkill, tgkill, kill(pid, 0)\n");
    long pid = sc(SYS_getpid, 0, 0, 0, 0, 0, 0);
    CHECK(sc(SYS_gettid, 0, 0, 0, 0, 0, 0) == pid, "gettid differs from getpid in a single-threaded process");
    CHECK(sc(SYS_kill, pid, 0, 0, 0, 0, 0) == 0, "kill(self, 0)");
    CHECK(sc(SYS_kill, 999999, 0, 0, 0, 0, 0) == -ESRCH_, "kill(nobody, 0)");
    CHECK(sc(SYS_tkill, pid, 0, 0, 0, 0, 0) == 0, "tkill(self, 0)");
    CHECK(sc(SYS_tkill, 999999, 0, 0, 0, 0, 0) == -ESRCH_, "tkill(nobody)");
    CHECK(sc(SYS_tkill, 0, 10, 0, 0, 0, 0) == -EINVAL_, "tkill(0)");
    CHECK(sc(SYS_tgkill, 0, pid, 10, 0, 0, 0) == -EINVAL_, "tgkill(tgid 0)");
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;
    sigaction(SIGUSR1, &sa, NULL);
    got_usr1 = 0;
    CHECK(sc(SYS_tgkill, pid, pid, SIGUSR1, 0, 0, 0) == 0, "tgkill(self, SIGUSR1)");
    CHECK(got_usr1 == 1, "handler ran %d times", got_usr1);
    CHECK(sc(SYS_tkill, pid, SIGUSR1, 0, 0, 0, 0) == 0 && got_usr1 == 2, "tkill(self, SIGUSR1): %d", got_usr1);
}

static void test_sigaltstack(void) {
    printf("sigaltstack\n");
    struct { void *sp; int flags; unsigned long size; } old = { (void *)1, 99, 99 }, ss = { 0 };
    CHECK(sc(SYS_sigaltstack, 0, (long)&old, 0, 0, 0, 0) == 0, "query");
    CHECK(old.flags == 2 && old.sp == 0 && old.size == 0, "flags %d (SS_DISABLE = 2)", old.flags);
    static char stack[16384];
    ss.sp = stack; ss.flags = 0; ss.size = sizeof stack;
    CHECK(sc(SYS_sigaltstack, (long)&ss, 0, 0, 0, 0, 0) == 0, "install");
    ss.flags = 0x40;
    CHECK(sc(SYS_sigaltstack, (long)&ss, 0, 0, 0, 0, 0) == -EINVAL_, "bad flags accepted");
}

static void test_clock_nanosleep(void) {
    printf("clock_nanosleep\n");
    struct timespec req = ts_ns(50 * 1000000LL);
    int64_t t0 = mono_ns();
    CHECK(sc(SYS_clock_nanosleep, CLOCK_MONOTONIC, 0, (long)&req, 0, 0, 0) == 0, "relative");
    int64_t dt = mono_ns() - t0;
    CHECK(dt >= 50 * 1000000LL && dt < 500 * 1000000LL, "slept %lld us", (long long)dt / 1000);
    t0 = mono_ns();
    req = ts_ns(t0 + 40 * 1000000LL);
    CHECK(sc(SYS_clock_nanosleep, CLOCK_MONOTONIC, 1, (long)&req, 0, 0, 0) == 0, "absolute");
    dt = mono_ns() - t0;
    CHECK(dt >= 40 * 1000000LL && dt < 500 * 1000000LL, "slept %lld us until an absolute time", (long long)dt / 1000);
    req = ts_ns(1000);
    t0 = mono_ns();
    long pr = sc(SYS_clock_nanosleep, CLOCK_MONOTONIC, 1, (long)&req, 0, 0, 0);
    dt = mono_ns() - t0;
    CHECK(pr == 0, "past absolute time returned %ld", pr);
    CHECK(dt < 2 * 1000000LL, "an absolute time in the past took %lld us (it must not even arm a timer)", (long long)dt / 1000);
    // CLOCK_REALTIME, absolute: 30 ms from now on the wall clock
    struct timespec now;
    clock_gettime(CLOCK_REALTIME, &now);
    req = ts_ns((int64_t)now.tv_sec * 1000000000 + now.tv_nsec + 30 * 1000000LL);
    t0 = mono_ns();
    CHECK(sc(SYS_clock_nanosleep, CLOCK_REALTIME, 1, (long)&req, 0, 0, 0) == 0, "realtime absolute");
    dt = mono_ns() - t0;
    CHECK(dt >= 25 * 1000000LL && dt < 500 * 1000000LL, "slept %lld us on CLOCK_REALTIME", (long long)dt / 1000);
    req.tv_sec = 0; req.tv_nsec = 1000000000;
    CHECK(sc(SYS_clock_nanosleep, CLOCK_MONOTONIC, 0, (long)&req, 0, 0, 0) == -EINVAL_, "tv_nsec = 1e9");
    req.tv_sec = -1; req.tv_nsec = 0;
    CHECK(sc(SYS_clock_nanosleep, CLOCK_MONOTONIC, 0, (long)&req, 0, 0, 0) == -EINVAL_, "negative tv_sec");
    req = ts_ns(1000);
    CHECK(sc(SYS_clock_nanosleep, CLOCK_MONOTONIC, 2, (long)&req, 0, 0, 0) == -EINVAL_, "unknown flag");
    CHECK(sc(SYS_clock_nanosleep, 99, 0, (long)&req, 0, 0, 0) == -EINVAL_, "unknown clock");
    CHECK(sc(SYS_clock_nanosleep, CLOCK_MONOTONIC, 0, 0, 0, 0, 0) < 0, "null request");
}

static int word;
static int word2;

static void test_futex_timeouts(void) {
    printf("futex: timeouts, values, bitsets\n");
    word = 0;
    struct timespec rel = ts_ns(30 * 1000000LL);
    int64_t t0 = mono_ns();
    long r = sc(SYS_futex, (long)&word, FUTEX_WAIT | FUTEX_PRIVATE, 0, (long)&rel, 0, 0);
    int64_t dt = mono_ns() - t0;
    CHECK(r == -ETIMEDOUT_, "returned %ld", r);
    CHECK(dt >= 30 * 1000000LL && dt < 500 * 1000000LL, "waited %lld us", (long long)dt / 1000);
    CHECK(sc(SYS_futex, (long)&word, FUTEX_WAIT | FUTEX_PRIVATE, 1, (long)&rel, 0, 0) == -EAGAIN_, "value mismatch");
    // WAIT_BITSET: an absolute deadline on CLOCK_MONOTONIC, MATCH_ANY
    struct timespec abs = ts_ns(mono_ns() + 30 * 1000000LL);
    t0 = mono_ns();
    r = sc(SYS_futex, (long)&word, FUTEX_WAIT_BITSET | FUTEX_PRIVATE, 0, (long)&abs, 0, 0xffffffffL);
    dt = mono_ns() - t0;
    CHECK(r == -ETIMEDOUT_ && dt >= 25 * 1000000LL && dt < 500 * 1000000LL, "returned %ld after %lld us", r, (long long)dt / 1000);
    struct timespec past = ts_ns(1000);
    t0 = mono_ns();
    r = sc(SYS_futex, (long)&word, FUTEX_WAIT_BITSET | FUTEX_PRIVATE, 0, (long)&past, 0, 0xffffffffL);
    dt = mono_ns() - t0;
    CHECK(r == -ETIMEDOUT_, "deadline in the past returned %ld", r);
    CHECK(dt < 2 * 1000000LL, "a deadline in the past took %lld us (it must not arm a timer)", (long long)dt / 1000);
    CHECK(sc(SYS_futex, (long)&word, FUTEX_WAIT_BITSET | FUTEX_PRIVATE, 0, (long)&abs, 0, 0) == -EINVAL_, "bitset 0");
    CHECK(sc(SYS_futex, (long)&word, FUTEX_WAKE_BITSET | FUTEX_PRIVATE, 1, 0, 0, 0) == -EINVAL_, "wake bitset 0");
    struct timespec bad = { 0, 1000000000 };
    CHECK(sc(SYS_futex, (long)&word, FUTEX_WAIT | FUTEX_PRIVATE, 0, (long)&bad, 0, 0) == -EINVAL_, "bad timespec");
    CHECK(sc(SYS_futex, (long)&word, FUTEX_WAKE | FUTEX_PRIVATE, 1, 0, 0, 0) == 0, "wake with no waiters");
}

// Waiters run in threads; `state` says where each is, so the main thread can wait for them to be asleep.
static volatile int asleep[4];
static volatile long result[4];
static uint32_t bitset_of[4];
static int *wait_on[4];

static void *waiter(void *arg) {
    int i = (int)(long)arg;
    asleep[i] = 1;
    struct timespec abs = ts_ns(mono_ns() + 1500 * 1000000LL);
    result[i] = sc(SYS_futex, (long)wait_on[i], FUTEX_WAIT_BITSET | FUTEX_PRIVATE, 0, (long)&abs, 0, bitset_of[i]);
    asleep[i] = 2;
    return NULL;
}

static void wait_asleep(int n) {
    for (int spin = 0; spin < 200; spin++) {
        int all = 1;
        for (int i = 0; i < n; i++) if (asleep[i] != 1) all = 0;
        if (all) break;
        usleep(5000);
    }
    usleep(50000); // they registered in the futex table after flagging
}

static void test_futex_threads(void) {
    printf("futex: WAKE_BITSET and REQUEUE with threads\n");
    pthread_t t[4];
    // bitsets: waiter 0 has mask 1, waiter 1 has mask 2
    word = 0;
    for (int i = 0; i < 2; i++) { asleep[i] = 0; result[i] = 12345; wait_on[i] = &word; bitset_of[i] = 1u << i; }
    for (long i = 0; i < 2; i++) pthread_create(&t[i], NULL, waiter, (void *)i);
    wait_asleep(2);
    CHECK(sc(SYS_futex, (long)&word, FUTEX_WAKE_BITSET | FUTEX_PRIVATE, 10, 0, 0, 4) == 0, "a mask nobody has woke someone");
    CHECK(asleep[0] == 1 && asleep[1] == 1, "waiters left");
    CHECK(sc(SYS_futex, (long)&word, FUTEX_WAKE_BITSET | FUTEX_PRIVATE, 10, 0, 0, 2) == 1, "mask 2 should wake exactly waiter 1");
    usleep(50000);
    CHECK(asleep[1] == 2 && asleep[0] == 1, "states %d %d", asleep[0], asleep[1]);
    CHECK(result[1] == 0, "waiter 1 returned %ld", result[1]);
    CHECK(sc(SYS_futex, (long)&word, FUTEX_WAKE | FUTEX_PRIVATE, 10, 0, 0, 0) == 1, "plain wake finds the last waiter");
    pthread_join(t[0], NULL);
    pthread_join(t[1], NULL);
    CHECK(result[0] == 0, "waiter 0 returned %ld", result[0]);

    // requeue: 3 waiters on `word`; CMP_REQUEUE wakes 1 and moves 2 to `word2`; a wake on `word` then finds none, on `word2` two
    word = 0; word2 = 0;
    for (int i = 0; i < 3; i++) { asleep[i] = 0; result[i] = 12345; wait_on[i] = &word; bitset_of[i] = 0xffffffffu; }
    for (long i = 0; i < 3; i++) pthread_create(&t[i], NULL, waiter, (void *)i);
    wait_asleep(3);
    CHECK(sc(SYS_futex, (long)&word, FUTEX_CMP_REQUEUE | FUTEX_PRIVATE, 1, 2, (long)&word2, 5) == -EAGAIN_, "CMP_REQUEUE with the wrong value");
    long moved = sc(SYS_futex, (long)&word, FUTEX_CMP_REQUEUE | FUTEX_PRIVATE, 1, 2, (long)&word2, 0);
    CHECK(moved == 3, "woke+requeued %ld (1 + 2)", moved);
    usleep(50000);
    int woke = (asleep[0] == 2) + (asleep[1] == 2) + (asleep[2] == 2);
    CHECK(woke == 1, "%d woken by the requeue, wanted 1", woke);
    CHECK(sc(SYS_futex, (long)&word, FUTEX_WAKE | FUTEX_PRIVATE, 10, 0, 0, 0) == 0, "waiters still on the old address");
    CHECK(sc(SYS_futex, (long)&word2, FUTEX_WAKE | FUTEX_PRIVATE, 10, 0, 0, 0) == 2, "the requeued two");
    for (int i = 0; i < 3; i++) pthread_join(t[i], NULL);
    CHECK(result[0] == 0 && result[1] == 0 && result[2] == 0, "results %ld %ld %ld", result[0], result[1], result[2]);
    CHECK(sc(SYS_futex, (long)&word, FUTEX_REQUEUE | FUTEX_PRIVATE, 1, 1, (long)&word2, 0) == 0, "REQUEUE with nobody waiting");
}

int main(void) {
    test_getrandom();
    test_tid_and_signals();
    test_sigaltstack();
    test_clock_nanosleep();
    test_futex_timeouts();
    test_futex_threads();
    if (failures) {
        printf("linux_abi_test: %d FAILED\n", failures);
        return 1;
    }
    printf("linux_abi_test: OK\n");
    return 0;
}
