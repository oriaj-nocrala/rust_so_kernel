// Wake-up latency: round trips between two processes over a pipe pair, a socketpair and eventfds, and how long a short nanosleep
// really takes. Prints numbers (microseconds per round trip); fails only above a generous ceiling. Not part of the default suite:
//   scripts/run-abi-suite.sh latency_bench      (QEMU_DEBUG_SMP=1 to compare with one CPU)
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <unistd.h>
#include <time.h>
#include <sys/socket.h>
#include <sys/wait.h>

#define printf(...) ((printf)(__VA_ARGS__), fflush(stdout))
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }
static long sc(long nr, long a, long b, long c) {
    long r;
    __asm__ volatile("syscall" : "=a"(r) : "a"(nr), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return r;
}
#define N 2000

// Parent sends a byte, child echoes it: N round trips. Returns us per round trip.
static double pingpong(int a_rd, int a_wr, int b_rd, int b_wr) {
    pid_t c = fork();
    if (c == 0) {
        char b;
        for (int i = 0; i < N; i++) { if (read(b_rd, &b, 1) != 1) _exit(1); if (write(b_wr, &b, 1) != 1) _exit(1); }
        _exit(0);
    }
    char b = 'x';
    double t0 = now();
    for (int i = 0; i < N; i++) { write(a_wr, &b, 1); read(a_rd, &b, 1); }
    double e = now() - t0;
    waitpid(c, NULL, 0);
    return e / N * 1e6;
}

int main(void) {
    int failed = 0;
    int p1[2], p2[2];
    pipe(p1); pipe(p2);
    double pipe_us = pingpong(p2[0], p1[1], p1[0], p2[1]);
    printf("LAT pipe ping-pong:        %8.1f us per round trip\n", pipe_us);

    int s[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, s);
    double sock_us = pingpong(s[0], s[0], s[1], s[1]);
    printf("LAT socketpair ping-pong:  %8.1f us per round trip\n", sock_us);

    // eventfds as the doorbells: 290 = eventfd2
    int e1 = (int)sc(290, 0, 0, 0), e2 = (int)sc(290, 0, 0, 0);
    pid_t c = fork();
    uint64_t v = 1;
    if (c == 0) { for (int i = 0; i < N; i++) { read(e1, &v, 8); v = 1; write(e2, &v, 8); } _exit(0); }
    double t0 = now();
    for (int i = 0; i < N; i++) { v = 1; write(e1, &v, 8); read(e2, &v, 8); }
    double ev_us = (now() - t0) / N * 1e6;
    waitpid(c, NULL, 0);
    printf("LAT eventfd ping-pong:     %8.1f us per round trip\n", ev_us);

    // sched_yield between two spinners is not a wake-up; nanosleep is: ask for 1 ms and 200 us, see what we get.
    for (int us = 1000; us >= 200; us -= 800) {
        struct timespec ts = {0, us * 1000L};
        double t = now();
        for (int i = 0; i < 100; i++) nanosleep(&ts, NULL);
        printf("LAT nanosleep(%4d us):     %8.1f us actual\n", us, (now() - t) / 100 * 1e6);
    }

    // one thread of work, no partner: the cost of the syscall itself
    t0 = now();
    for (int i = 0; i < 20000; i++) sc(39, 0, 0, 0);
    printf("LAT getpid syscall:        %8.2f us\n", (now() - t0) / 20000 * 1e6);

    if (pipe_us > 5000 || sock_us > 5000 || ev_us > 5000) { printf("latency_bench: FAIL (over 5 ms)\n"); failed = 1; }
    else printf("latency_bench: PASS\n");
    return failed;
}
