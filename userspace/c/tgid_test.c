// Thread groups: getpid() is the same in every thread and gettid() is not; getppid() and /proc/self are the group's; kill(pid)
// is process-directed (the leader takes it unless it blocks the signal, then another thread does), tkill/tgkill hit one thread
// (tgkill with the wrong group is ESRCH); a child forked by a thread is the *process's* child, waitable from the main thread.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <pthread.h>
#include <time.h>
#include <sys/wait.h>

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

static long sc(long nr, long a, long b, long c) {
    long r;
    __asm__ volatile("syscall" : "=a"(r) : "a"(nr), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return r;
}
static int gettid_raw(void) { return (int)sc(186, 0, 0, 0); }
static int tkill_raw(int tid, int sig) { return (int)sc(200, tid, sig, 0); }
static int tgkill_raw(int tgid, int tid, int sig) { return (int)sc(234, tgid, tid, sig); }

static volatile int handled, handled_tid;
static void handler(int sig) { (void)sig; handled_tid = gettid_raw(); handled = 1; }

static volatile int w_started, w_stop, w_pid, w_tid, w_ppid, w_child, w_fork_now;
static char w_self[64];

static void *worker(void *arg) {
    (void)arg;
    w_pid = getpid();
    w_tid = gettid_raw();
    w_ppid = getppid();
    ssize_t n = readlink("/proc/self", w_self, sizeof w_self - 1);
    w_self[n > 0 ? n : 0] = 0;
    w_started = 1;
    while (!w_stop) {
        if (w_fork_now) {
            w_fork_now = 0;
            pid_t c = fork();
            if (c == 0) _exit(getppid() == w_pid ? 0 : 1);
            w_child = c;
        }
        __asm__ volatile("pause");
    }
    return NULL;
}

static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }
static int wait_handled(void) {
    double end = now() + 3;
    while (!handled && now() < end) __asm__ volatile("pause");
    return handled;
}
static void arm(void) { handled = 0; handled_tid = 0; }

int main(void) {
    struct sigaction sa = {0};
    sa.sa_handler = handler;
    sigaction(SIGUSR1, &sa, NULL);

    int main_pid = getpid(), main_tid = gettid_raw();
    pthread_t t;
    pthread_create(&t, NULL, worker, NULL);
    while (!w_started) __asm__ volatile("pause");

    printf("ids\n");
    CHECK(main_tid == main_pid, "main tid %d pid %d", main_tid, main_pid);
    CHECK(w_pid == main_pid, "getpid in the thread is %d, wanted %d", w_pid, main_pid);
    CHECK(w_tid != main_pid && w_tid != 0, "gettid in the thread is %d", w_tid);
    CHECK(w_ppid == getppid(), "getppid in the thread %d, in main %d", w_ppid, getppid());
    char want[64];
    snprintf(want, sizeof want, "/proc/%d", main_pid);
    CHECK(strcmp(w_self, want) == 0, "/proc/self in the thread is '%s', wanted '%s'", w_self, want);

    printf("prctl(PR_SET_NAME / PR_GET_NAME) names the thread\n");
    char nm[16] = {0};
    CHECK(sc(157, 15, (long)"tgid-main", 0) == 0 && sc(157, 16, (long)nm, 0) == 0 && strcmp(nm, "tgid-main") == 0, "name read back '%s'", nm);
    sc(157, 15, (long)"a-name-longer-than-fifteen", 0);
    sc(157, 16, (long)nm, 0);
    CHECK(strcmp(nm, "a-name-longer-t") == 0, "a long name is cut to 15 bytes: '%s'", nm);
    CHECK(sc(157, 9999, 0, 0) == -22, "an unknown option is EINVAL");

    printf("uids: a single-user system, everyone is root\n");
    CHECK(sc(102, 0, 0, 0) == 0 && sc(104, 0, 0, 0) == 0 && sc(107, 0, 0, 0) == 0 && sc(108, 0, 0, 0) == 0, "getuid/getgid/geteuid/getegid");

    printf("kill(tid of a thread) is process-directed: the leader takes it\n");
    arm();
    CHECK(tkill_raw(w_tid, 0) == 0, "tkill sig 0 to the thread");
    CHECK(kill(w_tid, SIGUSR1) == 0, "kill(tid) succeeds");           // kill(tid of a thread) signals its group
    CHECK(wait_handled(), "no handler ran");
    CHECK(handled_tid == main_tid, "handled by tid %d, wanted the leader %d", handled_tid, main_tid);

    printf("the leader blocks SIGUSR1: another thread takes it\n");
    arm();
    sigset_t set, old;
    sigemptyset(&set); sigaddset(&set, SIGUSR1);
    pthread_sigmask(SIG_BLOCK, &set, &old);
    // the worker was created before the block, so it does not block it
    CHECK(kill(main_pid, SIGUSR1) == 0, "kill(getpid())");
    CHECK(wait_handled(), "no handler ran");
    CHECK(handled_tid == w_tid, "handled by tid %d, wanted the worker %d", handled_tid, w_tid);
    pthread_sigmask(SIG_SETMASK, &old, NULL);

    printf("tkill / tgkill hit one thread\n");
    arm();
    CHECK(tgkill_raw(main_pid, w_tid, SIGUSR1) == 0, "tgkill to the worker");
    CHECK(wait_handled(), "no handler ran");
    CHECK(handled_tid == w_tid, "handled by tid %d, wanted the worker %d, not the leader", handled_tid, w_tid);
    arm();
    CHECK(tkill_raw(w_tid, SIGUSR1) == 0, "tkill to the worker");
    CHECK(wait_handled(), "no handler ran");
    CHECK(handled_tid == w_tid, "handled by tid %d, wanted the worker %d", handled_tid, w_tid);
    CHECK(tgkill_raw(main_pid + 1000, w_tid, 0) == -3, "tgkill with the wrong group gives %d, wanted -ESRCH", tgkill_raw(main_pid + 1000, w_tid, 0));
    CHECK(tgkill_raw(main_pid, w_tid + 1000, 0) == -3, "tgkill of no thread gives %d", tgkill_raw(main_pid, w_tid + 1000, 0));

    printf("a child forked by a thread is the process's child\n");
    w_child = 0;
    w_fork_now = 1;
    double end = now() + 3;
    while (!w_child && now() < end) __asm__ volatile("pause");
    CHECK(w_child > 0, "the thread's fork gave %d", w_child);
    int st = -1;
    pid_t r = waitpid(-1, &st, 0);
    CHECK(r == w_child, "waitpid from the main thread got %d, wanted %d", r, w_child);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "the child saw getppid() != the group's pid (status 0x%x)", st);

    w_stop = 1;
    pthread_join(t, NULL);
    printf(failures ? "tgid_test: FAIL\n" : "tgid_test: PASS\n");
    return failures != 0;
}
