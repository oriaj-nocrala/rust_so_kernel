// testutil.h: helpers for the C test programs, so they wait for a condition
// instead of sleeping "long enough" (see the kernel-testing skill, "Writing
// tests that run fast").
//
// The pattern it replaces: fork a child that blocks in some call, then
// nanosleep(150 ms) "so it is asleep" before signalling it. That costs the
// full nap on every run and still races on a slow host. tu_wait_blocked()
// returns as soon as the kernel reports the task blocked, which is the
// condition the nap was standing in for.
#ifndef TESTUTIL_H
#define TESTUTIL_H

#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

static inline long tu_now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000L + ts.tv_nsec / 1000000L;
}

static inline void tu_nap_ms(long ms) {
    struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L };
    nanosleep(&ts, NULL);
}

// The state letter of /proc/<pid>/stat (R running or ready, S blocked,
// T stopped, Z zombie), or 0 when the task does not exist. Threads have
// pids of their own here, so this works for a thread's pid too.
static inline char tu_state(pid_t pid) {
    char path[32], buf[256];
    snprintf(path, sizeof path, "/proc/%d/stat", (int)pid);
    int fd = open(path, O_RDONLY);
    if (fd < 0) return 0;
    ssize_t n = read(fd, buf, sizeof buf - 1);
    close(fd);
    if (n <= 0) return 0;
    buf[n] = 0;
    // "pid (comm) S ...": comm may hold spaces or parentheses, so take the last ')'.
    char *p = strrchr(buf, ')');
    return p && p[1] == ' ' ? p[2] : 0;
}

// Waits until `pid` is in state `state`, polling every millisecond.
// Returns 1 when it got there, 0 after `timeout_ms` (the caller's check then
// fails with a message instead of the test hanging).
static inline int tu_wait_state(pid_t pid, char state, long timeout_ms) {
    long end = tu_now_ms() + timeout_ms;
    while (tu_state(pid) != state) {
        if (tu_now_ms() > end) return 0;
        tu_nap_ms(1);
    }
    return 1;
}

// Waits until `pid` is blocked in the kernel (a sleep, a read, a futex...).
// Only meaningful when the task has exactly one place it can block before
// the event the caller is waiting for.
static inline int tu_wait_blocked(pid_t pid, long timeout_ms) {
    return tu_wait_state(pid, 'S', timeout_ms);
}

// Waits until the wall clock is past second `t` (at most a second). File
// timestamps here have whole-second resolution and come from the same clock
// as time(), so a stamp taken after this is greater than `t`: what a fixed
// nanosleep of 1-2 s was approximating, at half the cost on average.
static inline void tu_wait_second_after(time_t t) {
    while (time(NULL) <= t) tu_nap_ms(5);
}

#endif
