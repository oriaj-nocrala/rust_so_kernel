// siginfo_t of signals sent by processes: kill() is SI_USER with the sender's pid, tkill/tgkill (so raise) is SI_TKILL, a
// SIGCHLD carries CLD_EXITED / CLD_KILLED / CLD_STOPPED with the child's pid and its status or signal, and a signal a child
// sends to its parent names the child.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <signal.h>
#include <time.h>
#include <sys/wait.h>

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

static long sc(long nr, long a, long b, long c) {
    long r;
    __asm__ volatile("syscall" : "=a"(r) : "a"(nr), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return r;
}

static volatile int got, got_sig, got_code, got_pid, got_status;
static void handler(int sig, siginfo_t *si, void *ctx) {
    (void)ctx;
    got_sig = sig; got_code = si->si_code; got_pid = si->si_pid; got_status = si->si_status;
    got = 1;
}
static void arm(void) { got = got_sig = got_code = got_pid = got_status = 0; }
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }
static int wait_got(void) {
    double end = now() + 3;
    while (!got && now() < end) __asm__ volatile("pause");
    return got;
}

static void child_cld(pid_t *child, int how) {
    arm();
    pid_t c = fork();
    if (c == 0) {
        if (how == 0) _exit(7);
        if (how == 1) kill(getpid(), SIGTERM);            // default action: killed by SIGTERM
        if (how == 2) { kill(getpid(), SIGSTOP); _exit(0); }
        for (;;) ;
    }
    *child = c;
}

int main(void) {
    struct sigaction sa = {0};
    sa.sa_sigaction = handler;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGUSR1, &sa, NULL);
    sigaction(SIGCHLD, &sa, NULL);
    int me = getpid();

    printf("kill() is SI_USER from the sender\n");
    arm();
    CHECK(kill(me, SIGUSR1) == 0, "kill");
    CHECK(wait_got() && got_sig == SIGUSR1, "no SIGUSR1 handler ran");
    CHECK(got_code == SI_USER, "si_code %d, wanted SI_USER (0)", got_code);
    CHECK(got_pid == me, "si_pid %d, wanted %d", got_pid, me);

    printf("kill(0) to the own group is SI_USER too\n");
    arm();
    CHECK(kill(0, SIGUSR1) == 0, "kill(0)");
    CHECK(wait_got(), "no handler ran");
    CHECK(got_code == SI_USER && got_pid == me, "si_code %d si_pid %d", got_code, got_pid);

    printf("tgkill is SI_TKILL\n");
    arm();
    CHECK(sc(234, me, me, SIGUSR1) == 0, "tgkill");
    CHECK(wait_got(), "no handler ran");
    CHECK(got_code == -6, "si_code %d, wanted SI_TKILL (-6)", got_code);
    CHECK(got_pid == me, "si_pid %d, wanted %d", got_pid, me);

    printf("a child kills its parent: si_pid is the child\n");
    sigset_t chld, old;                                    // the child's exit SIGCHLD must not overwrite what SIGUSR1 recorded
    sigemptyset(&chld); sigaddset(&chld, SIGCHLD);
    sigprocmask(SIG_BLOCK, &chld, &old);
    arm();
    pid_t c = fork();
    if (c == 0) { kill(getppid(), SIGUSR1); _exit(0); }
    CHECK(wait_got() && got_sig == SIGUSR1, "no SIGUSR1 (got signal %d)", got_sig);
    CHECK(got_code == SI_USER, "si_code %d", got_code);
    CHECK(got_pid == c, "si_pid %d, wanted the child %d", got_pid, c);
    waitpid(c, NULL, 0);
    sigprocmask(SIG_SETMASK, &old, NULL);                  // the pending SIGCHLD runs its handler here

    printf("SIGCHLD: the child exits with 7\n");
    child_cld(&c, 0);
    CHECK(wait_got() && got_sig == SIGCHLD, "no SIGCHLD");
    CHECK(got_code == CLD_EXITED, "si_code %d, wanted CLD_EXITED (1)", got_code);
    CHECK(got_pid == c, "si_pid %d, wanted %d", got_pid, c);
    CHECK(got_status == 7, "si_status %d, wanted 7", got_status);
    waitpid(c, NULL, 0);

    printf("SIGCHLD: the child is killed by SIGTERM\n");
    child_cld(&c, 1);
    CHECK(wait_got() && got_sig == SIGCHLD, "no SIGCHLD");
    CHECK(got_code == CLD_KILLED, "si_code %d, wanted CLD_KILLED (2)", got_code);
    CHECK(got_pid == c && got_status == SIGTERM, "si_pid %d si_status %d, wanted %d and %d", got_pid, got_status, c, SIGTERM);
    waitpid(c, NULL, 0);

    printf("SIGCHLD: the child stops\n");
    child_cld(&c, 2);
    CHECK(wait_got() && got_sig == SIGCHLD, "no SIGCHLD");
    CHECK(got_code == CLD_STOPPED, "si_code %d, wanted CLD_STOPPED (5)", got_code);
    CHECK(got_pid == c && got_status == SIGSTOP, "si_pid %d si_status %d, wanted %d and %d", got_pid, got_status, c, SIGSTOP);
    kill(c, SIGKILL);
    waitpid(c, NULL, 0);

    printf(failures ? "siginfo_test: FAIL\n" : "siginfo_test: PASS\n");
    return failures != 0;
}
