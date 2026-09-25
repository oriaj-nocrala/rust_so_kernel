// wait_intr_test: a signal ends a blocked wait (process::wait in the
// kernel), and nothing else goes wrong because of it.
//
// Seven kinds of wait: nanosleep, pipe read, pipe write (full pipe), futex,
// waitpid, poll, socket read. For each:
//   A. SIGKILL ends it at once (it used to wait for the natural wakeup: a
//      SIGKILL to `sleep 100` took 100 s).
//   B. A handler without SA_RESTART: the call fails with EINTR after the
//      handler ran.
//   C. A handler with SA_RESTART: read/write/futex/waitpid are re-executed
//      and complete with their real result; nanosleep and poll still EINTR
//      (Linux's ERESTARTNOHAND).
// Then what the interruption leaves behind:
//   D. Stale registrations do not act: bytes written to a pipe whose reader
//      was interrupted stay in the pipe (not handed to a reader that left),
//      a sleep's timer does not end the next wait early, and a socket that
//      becomes readable does not end an unrelated later sleep.
//   E. SIGSTOP stops a sleeping process at once; after SIGCONT the call
//      goes on as if nothing happened (a stop is invisible to it).
//   F. fork() inherits handlers, their SA_RESTART and the signal mask.
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/socket.h>
#include <sys/wait.h>

static int fails;
static volatile sig_atomic_t handled;
// Threads have pids of their own here, so a helper thread's getpid() is
// not the process it should signal.
static pid_t main_pid;

static void check(int ok, const char *what) {
    printf("%s: %s\n", what, ok ? "PASS" : "FAIL");
    if (!ok) fails++;
}

static long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000L + ts.tv_nsec / 1000000L;
}

static void nap_ms(long ms) {
    struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L };
    nanosleep(&ts, NULL);
}

static void on_usr1(int sig) { (void)sig; handled++; }

static void install(int sig, void (*fn)(int), int flags) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = fn;
    sa.sa_flags = flags;
    sigaction(sig, &sa, NULL);
}

static long raw_futex(volatile int *addr, int op, int val) {
    long ret;
    register long r10 asm("r10") = 0;
    asm volatile("syscall" : "=a"(ret)
                 : "a"(202L), "D"(addr), "S"((long)op), "d"((long)val), "r"(r10)
                 : "rcx", "r11", "memory");
    return ret;
}

// ── The seven waits ─────────────────────────────────────────────────────
// Each blocks once and returns the call's result (-errno on failure).
// `peer` is what the other side can use to complete it; setup() makes it.

enum kind { SLEEP, PIPE_READ, PIPE_WRITE, FUTEX, WAITPID, POLL, SOCK_READ, NKINDS };
static const char *kind_name[] = {
    "nanosleep", "pipe read", "pipe write", "futex", "waitpid", "poll", "socket read",
};
// Linux: re-executed under SA_RESTART?
static const int kind_restarts[] = { 0, 1, 1, 1, 1, 0, 1 };

struct ctx {
    int p[2];          // pipe
    int s[2];          // socketpair
    volatile int word; // futex word
    pid_t child;       // for WAITPID: a child that exits by itself
};

static void setup(enum kind k, struct ctx *c) {
    memset(c, 0, sizeof *c);
    c->p[0] = c->p[1] = c->s[0] = c->s[1] = -1;
    if (k == PIPE_READ || k == PIPE_WRITE) pipe(c->p);
    if (k == PIPE_WRITE) {
        static char fill[4096];
        write(c->p[1], fill, sizeof fill); // full: the next write blocks
    }
    // poll() on a socket: this kernel's poll reports a pipe as always ready.
    if (k == SOCK_READ || k == POLL) socketpair(AF_UNIX, SOCK_STREAM, 0, c->s);
    if (k == WAITPID) {
        c->child = fork();
        if (c->child == 0) { nap_ms(400); _exit(7); }
    }
}

static void teardown(struct ctx *c) {
    for (int i = 0; i < 2; i++) {
        if (c->p[i] >= 0) close(c->p[i]);
        if (c->s[i] >= 0) close(c->s[i]);
    }
    if (c->child > 0) waitpid(c->child, NULL, 0);
}

static long do_wait(enum kind k, struct ctx *c) {
    char b = 0;
    long r;
    switch (k) {
    case SLEEP: {
        struct timespec ts = { 2, 0 };
        r = nanosleep(&ts, NULL);
        break;
    }
    case PIPE_READ: r = read(c->p[0], &b, 1); if (r == 1) r = b; break;
    case PIPE_WRITE: r = write(c->p[1], "w", 1); break;
    case FUTEX: return raw_futex(&c->word, 0 /* WAIT */, 0);
    case WAITPID: {
        int st = 0;
        r = waitpid(c->child, &st, 0);
        if (r == c->child) { c->child = 0; r = WEXITSTATUS(st); }
        break;
    }
    case POLL: {
        struct pollfd pf = { c->s[0], POLLIN, 0 };
        r = poll(&pf, 1, -1);
        break;
    }
    case SOCK_READ: r = read(c->s[0], &b, 1); if (r == 1) r = b; break;
    default: r = -1;
    }
    return r < 0 ? -errno : r;
}

// What completes each wait, from outside it, and the result it then gives.
static void complete(enum kind k, struct ctx *c) {
    char tmp[4096];
    switch (k) {
    case PIPE_READ: write(c->p[1], "\x2a", 1); break;
    case PIPE_WRITE: read(c->p[0], tmp, sizeof tmp); break;
    case FUTEX: c->word = 1; raw_futex(&c->word, 1 /* WAKE */, 1); break;
    case SOCK_READ: write(c->s[1], "\x2a", 1); break;
    default: break; // WAITPID: the child exits by itself
    }
}
static const long kind_result[] = { 0, 42, 1, 0, 7, 1, 42 };

// ── A: SIGKILL ends every wait ──────────────────────────────────────────

static void case_a(enum kind k) {
    struct ctx c;
    pid_t pid = fork();
    if (pid == 0) {
        // Set up inside the child: its waitpid needs a child of its own.
        setup(k, &c);
        do_wait(k, &c);
        _exit(0);
    }
    nap_ms(150);
    long t0 = now_ms();
    kill(pid, SIGKILL);
    int st = 0;
    waitpid(pid, &st, 0);
    long waited = now_ms() - t0;
    char what[64];
    snprintf(what, sizeof what, "A %s: SIGKILL ends it (%ldms)", kind_name[k], waited);
    check(WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL && waited < 500, what);
}

// ── B and C: a handler, with and without SA_RESTART ─────────────────────
// A helper thread (so futex has someone in its address space) sends
// SIGUSR1 at 100 ms and completes the wait at 250 ms.

struct helper { pid_t target; enum kind k; struct ctx *c; };

static void *helper_main(void *arg) {
    struct helper *h = arg;
    nap_ms(100);
    kill(h->target, SIGUSR1);
    nap_ms(150);
    complete(h->k, h->c);
    return NULL;
}

static void case_bc(enum kind k, int restart) {
    struct ctx c;
    setup(k, &c);
    install(SIGUSR1, on_usr1, restart ? SA_RESTART : 0);
    handled = 0;
    struct helper h = { getpid(), k, &c };
    pthread_t t;
    pthread_create(&t, NULL, helper_main, &h);
    long t0 = now_ms();
    long r = do_wait(k, &c);
    long waited = now_ms() - t0;
    pthread_join(t, NULL);
    int expect_restart = restart && kind_restarts[k];
    int ok = handled == 1;
    if (expect_restart) ok &= r == kind_result[k] && waited >= 200;
    else ok &= r == -EINTR && waited < 240;
    char what[96];
    snprintf(what, sizeof what, "%s %s: %s (r=%ld, %ldms)", restart ? "C" : "B", kind_name[k],
             expect_restart ? "restarted, completed" : "EINTR", r, waited);
    check(ok, what);
    teardown(&c);
    install(SIGUSR1, SIG_DFL, 0);
}

// ── D: what an interruption leaves behind does nothing ─────────────────

static void *usr1_at(void *arg) {
    nap_ms((long)(intptr_t)arg);
    kill(main_pid, SIGUSR1);
    return NULL;
}

static void case_d(void) {
    install(SIGUSR1, on_usr1, 0);

    // D1: a reader interrupted; bytes written later stay in the pipe for
    // whoever reads next, and do not end the sleep the ex-reader is in.
    {
        int p[2];
        pipe(p);
        pthread_t t;
        pthread_create(&t, NULL, usr1_at, (void *)(intptr_t)80);
        char b = 0;
        long r = read(p[0], &b, 1);
        int e = errno;
        pthread_join(t, NULL);
        pid_t w = fork();
        if (w == 0) { nap_ms(100); write(p[1], "Z", 1); _exit(0); }
        long t0 = now_ms();
        nap_ms(300);
        long slept = now_ms() - t0;
        waitpid(w, NULL, 0);
        long r2 = read(p[0], &b, 1);
        char what[96];
        snprintf(what, sizeof what, "D1 interrupted pipe reader: bytes stay, sleep %ldms", slept);
        check(r == -1 && e == EINTR && slept >= 290 && r2 == 1 && b == 'Z', what);
        close(p[0]);
        close(p[1]);
    }

    // D2: a sleep interrupted; its timer, still due, does not end the pipe
    // read that follows.
    {
        int p[2];
        pipe(p);
        pthread_t t;
        pthread_create(&t, NULL, usr1_at, (void *)(intptr_t)50);
        struct timespec ts = { 0, 200 * 1000000L };
        long r = nanosleep(&ts, NULL);
        int e = errno;
        pthread_join(t, NULL);
        pid_t w = fork();
        if (w == 0) { nap_ms(400); write(p[1], "Q", 1); _exit(0); }
        long t0 = now_ms();
        char b = 0;
        long r2 = read(p[0], &b, 1);
        long waited = now_ms() - t0;
        waitpid(w, NULL, 0);
        char what[96];
        snprintf(what, sizeof what, "D2 interrupted sleep: its timer leaves the next read alone (%ldms)", waited);
        check(r == -1 && e == EINTR && r2 == 1 && b == 'Q' && waited >= 350, what);
        close(p[0]);
        close(p[1]);
    }

    // D3: a poll on a socket interrupted; the socket turning readable later
    // does not end the sleep that follows.
    {
        int s[2];
        socketpair(AF_UNIX, SOCK_STREAM, 0, s);
        pthread_t t;
        pthread_create(&t, NULL, usr1_at, (void *)(intptr_t)50);
        struct pollfd pf = { s[0], POLLIN, 0 };
        long r = poll(&pf, 1, -1);
        int e = errno;
        pthread_join(t, NULL);
        pid_t w = fork();
        if (w == 0) { nap_ms(100); write(s[1], "S", 1); _exit(0); }
        long t0 = now_ms();
        nap_ms(300);
        long slept = now_ms() - t0;
        waitpid(w, NULL, 0);
        char what[96];
        snprintf(what, sizeof what, "D3 interrupted poll: socket event leaves the next sleep alone (%ldms)", slept);
        check(r == -1 && e == EINTR && slept >= 290, what);
        close(s[0]);
        close(s[1]);
    }
    install(SIGUSR1, SIG_DFL, 0);
}

// ── E: stop and continue are invisible to the call ─────────────────────

static void case_e(void) {
    // E1: a sleeping child stops at once and, continued, sleeps on.
    pid_t pid = fork();
    if (pid == 0) {
        struct timespec ts = { 0, 600 * 1000000L };
        _exit(nanosleep(&ts, NULL) == 0 ? 0 : 1);
    }
    nap_ms(100);
    long t0 = now_ms();
    kill(pid, SIGSTOP);
    int st = 0;
    int w = waitpid(pid, &st, WUNTRACED);
    long to_stop = now_ms() - t0;
    int stopped = w == pid && WIFSTOPPED(st);
    kill(pid, SIGCONT);
    waitpid(pid, &st, 0);
    char what[96];
    snprintf(what, sizeof what, "E1 SIGSTOP stops a sleeper at once (%ldms), SIGCONT resumes", to_stop);
    check(stopped && to_stop < 300 && WIFEXITED(st) && WEXITSTATUS(st) == 0, what);

    // E2: a pipe reader stopped and continued still gets its byte.
    int p[2];
    pipe(p);
    pid = fork();
    if (pid == 0) {
        char b = 0;
        long r = read(p[0], &b, 1);
        _exit(r == 1 && b == 'C' ? 0 : 1);
    }
    nap_ms(100);
    kill(pid, SIGSTOP);
    w = waitpid(pid, &st, WUNTRACED);
    stopped = w == pid && WIFSTOPPED(st);
    kill(pid, SIGCONT);
    nap_ms(50);
    write(p[1], "C", 1);
    waitpid(pid, &st, 0);
    check(stopped && WIFEXITED(st) && WEXITSTATUS(st) == 0, "E2 stopped pipe reader continues and reads");
    close(p[0]);
    close(p[1]);
}

// ── F: fork inherits dispositions and the mask ─────────────────────────

static void case_f(void) {
    install(SIGUSR1, SIG_IGN, 0);
    install(SIGUSR2, on_usr1, SA_RESTART);
    sigset_t block, old;
    sigemptyset(&block);
    sigaddset(&block, SIGTERM);
    sigprocmask(SIG_BLOCK, &block, &old);

    pid_t pid = fork();
    if (pid == 0) {
        handled = 0;
        // Ignored in the parent, so ignored here: survives.
        kill(getpid(), SIGUSR1);
        // Caught in the parent, so caught here.
        kill(getpid(), SIGUSR2);
        // SA_RESTART inherited: a pipe read interrupted by SIGUSR2 goes on.
        int p[2];
        pipe(p);
        pid_t w = fork();
        if (w == 0) {
            nap_ms(100);
            kill(getppid(), SIGUSR2);
            nap_ms(100);
            write(p[1], "R", 1);
            _exit(0);
        }
        char b = 0;
        long r = read(p[0], &b, 1);
        waitpid(w, NULL, 0);
        sigset_t cur;
        sigprocmask(SIG_BLOCK, NULL, &cur);
        int ok = handled == 2 && r == 1 && b == 'R' && sigismember(&cur, SIGTERM);
        _exit(ok ? 0 : 10 + handled);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    char what[64];
    snprintf(what, sizeof what, "F fork inherits handlers, SA_RESTART, mask (st=%#x)", st);
    check(WIFEXITED(st) && WEXITSTATUS(st) == 0, what);

    sigprocmask(SIG_SETMASK, &old, NULL);
    install(SIGUSR1, SIG_DFL, 0);
    install(SIGUSR2, SIG_DFL, 0);
}

// `wait_intr_test [ABCDEF]...` runs only the named cases (for sabotage
// runs); no argument runs them all.
int main(int argc, char **argv) {
    main_pid = getpid();
    const char *only = argc > 1 ? argv[1] : "ABCDEF";
    if (strchr(only, 'A')) for (int k = 0; k < NKINDS; k++) case_a(k);
    if (strchr(only, 'B')) for (int k = 0; k < NKINDS; k++) case_bc(k, 0);
    if (strchr(only, 'C')) for (int k = 0; k < NKINDS; k++) case_bc(k, 1);
    if (strchr(only, 'D')) case_d();
    if (strchr(only, 'E')) case_e();
    if (strchr(only, 'F')) case_f();
    printf("wait_intr_test: %s (%d failed)\n", fails ? "FAIL" : "PASS", fails);
    return fails ? 1 : 0;
}
