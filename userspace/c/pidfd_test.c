// pidfd_open(434) / pidfd_send_signal(424): a pidfd is readable once its process has exited, however it died (exit, SIGKILL through the
// pidfd, a fatal fault) and even when the poller is already asleep; ESRCH / EINVAL cases; dup; close-on-exec; poll() and epoll.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <errno.h>
#include <unistd.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <time.h>
#include <pthread.h>
#include <sys/wait.h>

#define printf(...) ((printf)(__VA_ARGS__), fflush(stdout))
static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

struct epoll_event { uint32_t events; uint64_t data; } __attribute__((packed));
#define EPOLLIN 1u
static long sc(long nr, long a, long b, long c, long d) {
    long r;
    register long r10 __asm__("r10") = d;
    __asm__ volatile("syscall" : "=a"(r) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10) : "rcx", "r11", "memory");
    return r;
}
static int pidfd_open(pid_t pid, unsigned flags) { return (int)sc(434, pid, flags, 0, 0); }
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }
static void ms(int n) { struct timespec ts = {n / 1000, (n % 1000) * 1000000L}; nanosleep(&ts, NULL); }

// A process that ends 150 ms from now in the given way.
enum { EXIT7, SEGV, SPIN };
static pid_t child(int how) {
    pid_t c = fork();
    if (c == 0) {
        if (how == SPIN) for (;;) ms(50);
        ms(150);
        if (how == SEGV) *(volatile int *)0x10 = 1;
        _exit(7);
    }
    return c;
}

// Wait on `pidfd` in a fresh epoll for up to 3 s. Returns events (0 on timeout) and how long it took.
static int wait_readable(int pidfd, double *took) {
    int ep = (int)sc(291, 0, 0, 0, 0);
    struct epoll_event ev = { .events = EPOLLIN, .data = 5 }, out[2];
    sc(233, ep, 1, pidfd, (long)&ev);
    double t0 = now();
    int n = (int)sc(232, ep, (long)out, 2, 3000);
    *took = now() - t0;
    close(ep);
    return n;
}

static void *thread_fn(void *arg) { *(int *)arg = (int)sc(186, 0, 0, 0, 0); ms(300); return NULL; }

int main(void) {
    double took;
    int st;

    printf("a child that exits: not readable before, readable after; a blocked epoll_wait is woken\n");
    pid_t c = child(EXIT7);
    int pfd = pidfd_open(c, 0);
    CHECK(pfd >= 0, "pidfd_open gave %d", pfd);
    CHECK((fcntl(pfd, F_GETFD) & FD_CLOEXEC) != 0, "a pidfd must be close-on-exec");
    struct pollfd p = { .fd = pfd, .events = POLLIN };
    CHECK(poll(&p, 1, 0) == 0, "readable while the child still runs");
    int n = wait_readable(pfd, &took);
    CHECK(n == 1 && took >= 0.1 && took < 1.5, "epoll_wait: n=%d after %.3f s, wanted 1 after about 0.15", n, took);
    CHECK(poll(&p, 1, 0) == 1 && (p.revents & POLLIN), "poll() after the exit: revents %x", p.revents);
    CHECK(waitpid(c, &st, 0) == c && WIFEXITED(st) && WEXITSTATUS(st) == 7, "waitpid status 0x%x", st);
    CHECK(poll(&p, 1, 0) == 1, "still readable after the child was reaped");
    close(pfd);

    printf("a fatal fault ends the child while the poller sleeps\n");
    c = child(SEGV);
    pfd = pidfd_open(c, 0);
    n = wait_readable(pfd, &took);
    CHECK(n == 1 && took < 1.5, "epoll_wait: n=%d after %.3f s (a timeout is 3 s)", n, took);
    CHECK(waitpid(c, &st, 0) == c && WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV, "waitpid status 0x%x", st);
    close(pfd);

    printf("pidfd_send_signal\n");
    c = child(SPIN);
    pfd = pidfd_open(c, 0);
    CHECK(sc(424, pfd, 0, 0, 0) == 0, "signal 0 probe");
    CHECK(sc(424, pfd, SIGKILL, 0, 0) == 0, "SIGKILL through the pidfd");
    n = wait_readable(pfd, &took);
    CHECK(n == 1 && took < 1.5, "readable after the kill: n=%d after %.3f s", n, took);
    CHECK(waitpid(c, &st, 0) == c && WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL, "waitpid status 0x%x", st);
    CHECK(sc(424, pfd, SIGKILL, 0, 0) == -3, "sending to an exited process gave %ld, wanted -ESRCH", sc(424, pfd, SIGKILL, 0, 0));
    CHECK(sc(424, 0, SIGKILL, 0, 0) == -9, "a non-pidfd (stdin) gave %ld, wanted -EBADF", sc(424, 0, SIGKILL, 0, 0));
    close(pfd);

    printf("errors\n");
    CHECK(pidfd_open(0, 0) == -22 && pidfd_open(-5, 0) == -22, "pid 0 / negative");
    CHECK(pidfd_open(getpid(), 0x1) == -22, "an unknown flag");
    CHECK(pidfd_open(999999, 0) == -3, "a pid that does not exist gave %d, wanted -ESRCH", pidfd_open(999999, 0));
    int tid = 0;
    pthread_t t;
    pthread_create(&t, NULL, thread_fn, &tid);
    ms(50);
    CHECK(tid > 0 && pidfd_open(tid, 0) == -22, "a thread's tid gave %d, wanted -EINVAL (only leaders)", pidfd_open(tid, 0));
    pthread_join(t, NULL);
    int self = pidfd_open(getpid(), 0);
    CHECK(self >= 0 && (poll(&(struct pollfd){ .fd = self, .events = POLLIN }, 1, 0) == 0), "a pidfd of the caller is not readable");
    close(self);

    printf("an already exited (zombie) child is readable at once; dup shares the state; NONBLOCK\n");
    c = child(EXIT7);
    ms(400);
    pfd = pidfd_open(c, 0x800);
    p.fd = pfd;
    CHECK(pfd >= 0 && poll(&p, 1, 0) == 1, "a zombie's pidfd is not readable at once");
    int d = dup(pfd);
    close(pfd);
    p.fd = d;
    CHECK(d >= 0 && poll(&p, 1, 0) == 1, "the dup lost the state");
    CHECK((fcntl(d, F_GETFL) & O_NONBLOCK) != 0, "PIDFD_NONBLOCK not reported by F_GETFL");
    waitpid(c, NULL, 0);
    close(d);

    printf(failures ? "pidfd_test: FAIL\n" : "pidfd_test: PASS\n");
    return failures != 0;
}
