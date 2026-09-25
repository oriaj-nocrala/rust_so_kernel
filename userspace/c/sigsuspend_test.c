// sigsuspend_test: rt_sigsuspend(130), the call BusyBox ash's `wait`
// builtin sleeps in (waitproc: block every signal, then sigsuspend with
// the old mask until SIGCHLD's handler has run). Before the syscall
// existed, mlibc's sigsuspend() returned ENOSYS at once and ash spun
// forever with every signal blocked.
//
//   A. SIGCHLD from a child that exits later wakes it: -1/EINTR, the
//      handler ran, and the old mask (SIGCHLD blocked) is back afterwards;
//   B. a signal already pending when it is called: returns at once;
//   C. SIGUSR1 sent by another process wakes it;
//   D. an ignored signal does not: the child sends SIGUSR2 (SIG_IGN)
//      first and SIGUSR1 later — only SIGUSR1 may end the wait.
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/wait.h>

static volatile sig_atomic_t got_chld, got_usr1;

static void on_chld(int sig) { (void)sig; got_chld++; }
static void on_usr1(int sig) { (void)sig; got_usr1++; }

static void nap_ms(long ms) {
    struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L };
    nanosleep(&ts, NULL);
}

static long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000L + ts.tv_nsec / 1000000L;
}

static void install(int sig, void (*fn)(int)) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = fn;
    sigaction(sig, &sa, NULL);
}

static int case_sigchld(void) {
    sigset_t block, old, empty, after;
    sigemptyset(&block);
    sigaddset(&block, SIGCHLD);
    sigprocmask(SIG_BLOCK, &block, &old);
    got_chld = 0;

    pid_t pid = fork();
    if (pid == 0) { nap_ms(300); _exit(7); }

    sigemptyset(&empty);
    long t0 = now_ms();
    int r = sigsuspend(&empty);
    int e = errno;
    long waited = now_ms() - t0;
    sigprocmask(SIG_SETMASK, NULL, &after);
    int status = 0;
    waitpid(pid, &status, 0);
    sigprocmask(SIG_SETMASK, &old, NULL);

    int ok = r == -1 && e == EINTR && got_chld == 1 && sigismember(&after, SIGCHLD)
        && waited >= 200 && WIFEXITED(status) && WEXITSTATUS(status) == 7;
    printf("A sigchld: r=%d errno=%d handler=%d mask_restored=%d waited=%ldms -> %s\n",
           r, e, (int)got_chld, sigismember(&after, SIGCHLD), waited, ok ? "PASS" : "FAIL");
    return !ok;
}

static int case_already_pending(void) {
    sigset_t block, old, empty;
    sigemptyset(&block);
    sigaddset(&block, SIGUSR1);
    sigprocmask(SIG_BLOCK, &block, &old);
    got_usr1 = 0;
    kill(getpid(), SIGUSR1); // stays pending: blocked

    sigemptyset(&empty);
    long t0 = now_ms();
    int r = sigsuspend(&empty);
    int e = errno;
    long waited = now_ms() - t0;
    sigprocmask(SIG_SETMASK, &old, NULL);

    int ok = r == -1 && e == EINTR && got_usr1 == 1 && waited < 100;
    printf("B already pending: r=%d errno=%d handler=%d waited=%ldms -> %s\n",
           r, e, (int)got_usr1, waited, ok ? "PASS" : "FAIL");
    return !ok;
}

static int case_kill_from_child(int ignored_first) {
    sigset_t block, old, empty;
    sigemptyset(&block);
    sigaddset(&block, SIGUSR1);
    sigaddset(&block, SIGUSR2);
    sigprocmask(SIG_BLOCK, &block, &old);
    got_usr1 = 0;

    pid_t parent = getpid();
    pid_t pid = fork();
    if (pid == 0) {
        nap_ms(200);
        if (ignored_first) {
            kill(parent, SIGUSR2);
            nap_ms(300);
        }
        kill(parent, SIGUSR1);
        _exit(0);
    }

    sigemptyset(&empty);
    long t0 = now_ms();
    int r = sigsuspend(&empty);
    int e = errno;
    long waited = now_ms() - t0;
    waitpid(pid, NULL, 0);
    sigprocmask(SIG_SETMASK, &old, NULL);

    long min_wait = ignored_first ? 400 : 100;
    int ok = r == -1 && e == EINTR && got_usr1 == 1 && waited >= min_wait;
    printf("%s: r=%d errno=%d handler=%d waited=%ldms -> %s\n",
           ignored_first ? "D ignored doesn't wake" : "C kill from child",
           r, e, (int)got_usr1, waited, ok ? "PASS" : "FAIL");
    return !ok;
}

int main(void) {
    install(SIGCHLD, on_chld);
    install(SIGUSR1, on_usr1);
    install(SIGUSR2, SIG_IGN);

    int fails = 0;
    fails += case_sigchld();
    fails += case_already_pending();
    fails += case_kill_from_child(0);
    fails += case_kill_from_child(1);
    printf("sigsuspend_test: %s\n", fails ? "FAIL" : "PASS");
    return fails ? 1 : 0;
}
