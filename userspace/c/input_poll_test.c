// input_poll_test: poll/epoll on /dev/input/event* report real readiness.
// Phase 2.2 of docs/gui/gui-plan.md.
//
// Before, every device fd but stdin was "always ready": a compositor that
// waited on the mouse with poll() spun at 100 %. Now an evdev fd is
// readable only when its queue (or its own handle) holds something, and
// the producers (keyboard ISR, IRQ12, the USB poll) wake whoever waits.
//
// With no argument, the checks that need no input from outside:
//   1. After draining, poll(event0/event1, 0) is 0.
//   2. poll(event0+event1, 150 ms) sleeps its timeout and returns 0.
//   3. POLLOUT on an input device is ready at once (Linux's evdev_poll).
//   4. epoll: epoll_wait(0) is 0; epoll_wait(150 ms) sleeps and returns 0.
//   5. A signal with a handler ends a poll on event1 with EINTR.
//
// `input_poll_test wait [epoll]` blocks up to 10 s on both devices and
// reports what woke it, for the host to drive:
//   scripts/qemu-debug.sh send "input_poll_test wait" && scripts/qemu-debug.sh enter
//   scripts/qemu-debug.sh mouse-move 10 5
// It prints "woken: event1 after N ms, R records" — N far below 10000 is
// the wakeup coming from the producer, not from the timeout.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/wait.h>

static int fails;

static void check(int ok, const char *what) {
    printf("%s: %s\n", what, ok ? "PASS" : "FAIL");
    if (!ok) fails++;
}

static long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000L + ts.tv_nsec / 1000000L;
}

// ── epoll by raw syscall: this mlibc port has no <sys/epoll.h> ──────────

struct epoll_event_packed {
    uint32_t events;
    uint64_t data;
} __attribute__((packed));

#define EPOLLIN 1
#define EPOLL_CTL_ADD 1

static long sys4(long nr, long a, long b, long c, long d) {
    long ret;
    register long r10 asm("r10") = d;
    asm volatile("syscall" : "=a"(ret)
                 : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10)
                 : "rcx", "r11", "memory");
    return ret;
}

static long epoll_create_raw(void) { return sys4(213, 1, 0, 0, 0); }
static long epoll_ctl_raw(int ep, int op, int fd, struct epoll_event_packed *ev) {
    return sys4(233, ep, op, fd, (long)ev);
}
static long epoll_wait_raw(int ep, struct epoll_event_packed *evs, int max, int timeout) {
    return sys4(232, ep, (long)evs, max, timeout);
}

// ── helpers ──────────────────────────────────────────────────────────────

// Read every record queued so far; returns how many.
static int drain(int fd) {
    char rec[24];
    int n = 0;
    while (read(fd, rec, sizeof rec) == (ssize_t)sizeof rec) n++;
    return n;
}

static int open_dev(const char *path) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        printf("open %s: %s\n", path, strerror(errno));
        printf("FAIL\n");
        exit(1);
    }
    return fd;
}

static void on_usr1(int sig) { (void)sig; }

// ── the host-driven half ─────────────────────────────────────────────────

static int wait_mode(int kbd, int mouse, int use_epoll) {
    drain(kbd);
    drain(mouse);
    printf("waiting up to 10 s on event0+event1 (%s)\n", use_epoll ? "epoll" : "poll");
    long t0 = now_ms();
    int kbd_ready = 0, mouse_ready = 0;
    long r;
    if (use_epoll) {
        int ep = (int)epoll_create_raw();
        struct epoll_event_packed ev = { EPOLLIN, 0 };
        epoll_ctl_raw(ep, EPOLL_CTL_ADD, kbd, &ev);
        ev.data = 1;
        epoll_ctl_raw(ep, EPOLL_CTL_ADD, mouse, &ev);
        struct epoll_event_packed out[2];
        r = epoll_wait_raw(ep, out, 2, 10000);
        for (long i = 0; i < r; i++) {
            if (out[i].data == 0) kbd_ready = 1; else mouse_ready = 1;
        }
    } else {
        struct pollfd fds[2] = { { kbd, POLLIN, 0 }, { mouse, POLLIN, 0 } };
        r = poll(fds, 2, 10000);
        kbd_ready = (fds[0].revents & POLLIN) != 0;
        mouse_ready = (fds[1].revents & POLLIN) != 0;
    }
    long dt = now_ms() - t0;
    int records = drain(kbd) + drain(mouse);
    printf("woken: %s%s%s after %ld ms (ret %ld), %d records\n",
           kbd_ready ? "event0" : "", kbd_ready && mouse_ready ? "+" : "",
           mouse_ready ? "event1" : (kbd_ready ? "" : "nothing"), dt, r, records);
    int ok = r > 0 && dt < 9000 && records > 0;
    printf("%s\n", ok ? "PASS" : "FAIL");
    return ok ? 0 : 1;
}

int main(int argc, char **argv) {
    int kbd = open_dev("/dev/input/event0");
    int mouse = open_dev("/dev/input/event1");

    if (argc > 1 && strcmp(argv[1], "wait") == 0)
        return wait_mode(kbd, mouse, argc > 2 && strcmp(argv[2], "epoll") == 0);

    drain(kbd);
    drain(mouse);

    // 1. Nothing queued → not readable.
    struct pollfd one = { mouse, POLLIN, 0 };
    check(poll(&one, 1, 0) == 0 && one.revents == 0, "1a poll(event1, 0) with nothing queued is 0");
    one = (struct pollfd){ kbd, POLLIN, 0 };
    check(poll(&one, 1, 0) == 0 && one.revents == 0, "1b poll(event0, 0) with nothing queued is 0");

    // 2. A timeout is slept, not spun through.
    struct pollfd both[2] = { { kbd, POLLIN, 0 }, { mouse, POLLIN, 0 } };
    long t0 = now_ms();
    int r = poll(both, 2, 150);
    long dt = now_ms() - t0;
    printf("   poll(150 ms) returned %d after %ld ms\n", r, dt);
    check(r == 0 && dt >= 140, "2  poll(event0+event1, 150 ms) sleeps its timeout");

    // 3. Writing is always possible.
    one = (struct pollfd){ mouse, POLLOUT, 0 };
    check(poll(&one, 1, 0) == 1 && one.revents == POLLOUT, "3  POLLOUT on event1 is ready at once");

    // 4. epoll, same answers.
    int ep = (int)epoll_create_raw();
    struct epoll_event_packed ev = { EPOLLIN, 7 };
    check(ep >= 0 && epoll_ctl_raw(ep, EPOLL_CTL_ADD, mouse, &ev) == 0, "4a epoll_create + EPOLL_CTL_ADD event1");
    struct epoll_event_packed out[4];
    check(epoll_wait_raw(ep, out, 4, 0) == 0, "4b epoll_wait(0) with nothing queued is 0");
    t0 = now_ms();
    long er = epoll_wait_raw(ep, out, 4, 150);
    dt = now_ms() - t0;
    printf("   epoll_wait(150 ms) returned %ld after %ld ms\n", er, dt);
    check(er == 0 && dt >= 140, "4c epoll_wait(150 ms) sleeps its timeout");
    close(ep);

    // 5. A signal ends the wait.
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;
    sigaction(SIGUSR1, &sa, NULL);
    pid_t parent = getpid();
    pid_t child = fork();
    if (child == 0) {
        usleep(100 * 1000);
        kill(parent, SIGUSR1);
        _exit(0);
    }
    one = (struct pollfd){ mouse, POLLIN, 0 };
    t0 = now_ms();
    r = poll(&one, 1, 5000);
    int err = errno;
    dt = now_ms() - t0;
    waitpid(child, NULL, 0);
    printf("   poll(event1, 5 s) returned %d (errno %d) after %ld ms\n", r, r < 0 ? err : 0, dt);
    check(r == -1 && err == EINTR && dt < 2000, "5  SIGUSR1 ends poll(event1) with EINTR");

    printf("%s (%d failed)\n", fails ? "FAIL" : "PASS", fails);
    return fails ? 1 : 0;
}
