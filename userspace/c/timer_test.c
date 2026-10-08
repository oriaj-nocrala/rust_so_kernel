// Timer resolution and the 100 Hz tick. The LAPIC timer is a one-shot clock event (interrupts::apic): a timeout shorter than a
// tick fires at its own time, not at the next tick — nanosleep(200 us) used to take 10 ms — while the tick itself (time slices,
// CPU-time accounting, /proc/kdebug's `timer_ticks`) must stay at 100 per second however many timeouts fire in between.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <poll.h>
#include <time.h>
#include <pthread.h>
#include <sys/wait.h>

#define printf(...) ((printf)(__VA_ARGS__), fflush(stdout))
static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

static long sc(long nr, long a, long b, long c, long d) {
    long r;
    register long r10 __asm__("r10") = d;
    __asm__ volatile("syscall" : "=a"(r) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10) : "rcx", "r11", "memory");
    return r;
}
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }

// Mean of n sleeps of `us` microseconds, in microseconds.
static double mean_sleep_us(long us, int n) {
    struct timespec ts = {0, us * 1000L};
    double t = now();
    for (int i = 0; i < n; i++) nanosleep(&ts, NULL);
    return (now() - t) / n * 1e6;
}

// timer_ticks and uptime_ms from /proc/kdebug ("timer_ticks: N over M ms of uptime").
static int ticks_now(long *ticks, long *ms) {
    static char buf[16384];
    int fd = open("/proc/kdebug", O_RDONLY);
    if (fd < 0) return -1;
    int n = read(fd, buf, sizeof buf - 1);
    close(fd);
    if (n <= 0) return -1;
    buf[n] = 0;
    char *p = strstr(buf, "timer_ticks:");
    return p && sscanf(p, "timer_ticks: %ld over %ld ms", ticks, ms) == 2 ? 0 : -1;
}

static void *sleeper(void *arg) {
    struct timespec ts = {0, 1000000};
    for (int i = 0; i < 100; i++) nanosleep(&ts, NULL);
    *(double *)arg = now();
    return NULL;
}

// A 6 ms sleep runs while another thread does 1 ms sleeps back to back: after each early interrupt the timer must be re-armed for
// the 6 ms one (with a single CPU they share one LAPIC timer; if it were re-armed only for the next tick the long sleep would end at
// 10 ms).
static void *short_sleeps(void *arg) {
    (void)arg;
    struct timespec ts = {0, 1000000};
    for (int i = 0; i < 4; i++) nanosleep(&ts, NULL);
    return NULL;
}
static void *long_sleep(void *arg) {
    struct timespec ts = {0, 6000000};
    double t = now();
    nanosleep(&ts, NULL);
    *(double *)arg = (now() - t) * 1e3;
    return NULL;
}

int main(void) {
    printf("nanosleep below a tick fires at its own time (a tick is 10 ms)\n");
    double a = mean_sleep_us(200, 50), b = mean_sleep_us(1000, 50), c = mean_sleep_us(3000, 30);
    printf("  (mean of 200 us: %.0f us, 1 ms: %.0f us, 3 ms: %.0f us)\n", a, b, c);
    CHECK(a >= 200 && a < 3000, "nanosleep(200 us) took %.0f us on average", a);
    CHECK(b >= 1000 && b < 4000, "nanosleep(1 ms) took %.0f us on average", b);
    CHECK(c >= 3000 && c < 6000, "nanosleep(3 ms) took %.0f us on average", c);

    printf("poll and epoll timeouts\n");
    double t = now();
    for (int i = 0; i < 20; i++) poll(NULL, 0, 1);
    double pm = (now() - t) / 20 * 1e3;
    CHECK(pm >= 1.0 && pm < 4.0, "poll(NULL, 0, 1 ms) took %.2f ms on average", pm);
    int ep = (int)sc(291, 0, 0, 0, 0);
    char out[16];
    t = now();
    for (int i = 0; i < 20; i++) sc(232, ep, (long)out, 1, 2);
    double em = (now() - t) / 20 * 1e3;
    CHECK(em >= 2.0 && em < 5.0, "epoll_wait(2 ms) took %.2f ms on average", em);

    printf("the tick stays at 100 Hz while short timeouts fire in between\n");
    long t0, m0, t1, m1;
    CHECK(ticks_now(&t0, &m0) == 0, "cannot read timer_ticks from /proc/kdebug");
    double s = now();
    // Half a second: ~50 ticks, so one tick either way is 2% against a 15% margin.
    while (now() - s < 0.5) { struct timespec ts = {0, 300000}; nanosleep(&ts, NULL); }   // ~1500 early interrupts
    CHECK(ticks_now(&t1, &m1) == 0, "timer_ticks (second read)");
    double per_s = (double)(t1 - t0) * 1000.0 / (double)(m1 - m0);
    CHECK(per_s > 85 && per_s < 115, "%.1f ticks per second while sleeping 300 us at a time (wanted about 100)", per_s);

    printf("an armed timer survives earlier ones firing first\n");
    double worst = 0;
    for (int round = 0; round < 10; round++) {
        pthread_t l, sh;
        double ms = 0;
        pthread_create(&l, NULL, long_sleep, &ms);
        pthread_create(&sh, NULL, short_sleeps, NULL);
        pthread_join(l, NULL);
        pthread_join(sh, NULL);
        if (ms > worst) worst = ms;
    }
    CHECK(worst < 9.0, "a 6 ms sleep next to 1 ms sleeps took up to %.2f ms (a tick-late re-arm makes it 10)", worst);

    printf("several sleepers at once\n");
    pthread_t th[4];
    double done[4];
    t = now();
    for (int i = 0; i < 4; i++) pthread_create(&th[i], NULL, sleeper, &done[i]);
    for (int i = 0; i < 4; i++) pthread_join(th[i], NULL);
    double all = (now() - t) * 1e3;
    CHECK(all >= 100 && all < 400, "4 threads x 100 sleeps of 1 ms took %.0f ms", all);

    printf("a sleeper wakes promptly while the other CPUs are busy\n");
    int ncpu = 0;
    FILE *ci = fopen("/proc/cpuinfo", "r");
    for (char line[256]; ci && fgets(line, sizeof line, ci);) if (strncmp(line, "processor", 9) == 0) ncpu++;
    if (ci) fclose(ci);
    if (ncpu >= 2) {
        pid_t spin[32];
        int n = ncpu - 1;                       // every CPU but one is busy: the sleeper has that one
        for (int i = 0; i < n; i++) if ((spin[i] = fork()) == 0) { for (;;) __asm__ volatile("pause"); }
        double busy = mean_sleep_us(1000, 30);
        for (int i = 0; i < n; i++) { kill(spin[i], 9); waitpid(spin[i], NULL, 0); }
        CHECK(busy >= 1000 && busy < 15000, "nanosleep(1 ms) with %d spinners on %d CPUs took %.0f us on average", n, ncpu, busy);
    } else {
        printf("  (one CPU: skipped)\n");
    }

    printf(failures ? "timer_test: FAIL\n" : "timer_test: PASS\n");
    return failures != 0;
}
