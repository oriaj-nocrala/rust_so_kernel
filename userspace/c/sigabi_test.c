// Linux's rt_sigaction ABI end to end, through mlibc: SA_SIGINFO (siginfo and ucontext), sa_mask, SA_NODEFER, SA_RESETHAND,
// SA_ONSTACK with sigaltstack, and a handler that edits the ucontext to change where the interrupted code resumes.
// Signals to a running thread come from another thread (tgkill), so they can land in the middle of a spin loop.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <ucontext.h>
#include <pthread.h>
#include <sys/wait.h>

static long sc(long nr, long a, long b, long c) {
    long ret;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(nr), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return ret;
}

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

static volatile int hits, depth, max_depth, order_n;
static volatile int order[8];
static volatile int seen_signo, seen_code;
static volatile long seen_rip, seen_rsp_in_handler;
static volatile int uc_ok;
static volatile double fp_result;

static void info_handler(int sig, siginfo_t *info, void *ctx) {
    ucontext_t *uc = ctx;
    seen_signo = info->si_signo;
    seen_code = info->si_code;
    seen_rip = (long)uc->uc_mcontext.gregs[REG_RIP];
    uc_ok = ctx != NULL && info != NULL && sig == info->si_signo;
    // SSE in a handler: a misaligned stack would fault here (movaps)
    volatile double x = 1.5;
    fp_result = x * 2.0 + sig;
    hits++;
}

static void test_siginfo(void) {
    printf("SA_SIGINFO\n");
    struct sigaction sa = {0};
    sa.sa_sigaction = info_handler;
    sa.sa_flags = SA_SIGINFO;
    CHECK(sigaction(SIGUSR1, &sa, NULL) == 0, "sigaction");
    hits = 0;
    kill(getpid(), SIGUSR1);
    CHECK(hits == 1, "handler ran %d times", hits);
    CHECK(seen_signo == SIGUSR1, "si_signo %d", seen_signo);
    CHECK(seen_code == 0, "si_code %d (SI_USER)", seen_code);
    CHECK(uc_ok, "ucontext and siginfo arrive, sig matches");
    CHECK(seen_rip > 0x1000, "REG_RIP in the ucontext is %#lx", seen_rip);
    CHECK(fp_result == 3.0 + SIGUSR1, "SSE inside the handler: %f", fp_result);

    struct sigaction old;
    CHECK(sigaction(SIGUSR1, NULL, &old) == 0, "read back");
    CHECK(old.sa_sigaction == info_handler && (old.sa_flags & SA_SIGINFO), "handler and SA_SIGINFO read back (flags %#lx)", (unsigned long)old.sa_flags);
    CHECK(old.sa_restorer != NULL, "libc's restorer is stored");
    signal(SIGUSR1, SIG_DFL);
}

static void mask_handler(int sig) {
    (void)sig;
    order[order_n++] = 1;                 // SIGUSR1 handler starts
    kill(getpid(), SIGUSR2);              // blocked by sa_mask: must not run yet
    order[order_n++] = 2;
}
static void usr2_handler(int sig) {
    (void)sig;
    order[order_n++] = 3;
}

static void test_sa_mask(void) {
    printf("sa_mask\n");
    struct sigaction sa = {0};
    sa.sa_handler = mask_handler;
    sigemptyset(&sa.sa_mask);
    sigaddset(&sa.sa_mask, SIGUSR2);
    sigaction(SIGUSR1, &sa, NULL);
    struct sigaction sb = {0};
    sb.sa_handler = usr2_handler;
    sigaction(SIGUSR2, &sb, NULL);
    order_n = 0;
    kill(getpid(), SIGUSR1);
    CHECK(order_n == 3 && order[0] == 1 && order[1] == 2 && order[2] == 3, "order %d %d %d (n=%d), wanted 1 2 3", order[0], order[1], order[2], order_n);
    sigset_t cur;
    sigprocmask(SIG_BLOCK, NULL, &cur);
    CHECK(!sigismember(&cur, SIGUSR1) && !sigismember(&cur, SIGUSR2), "the mask is back to normal after the handler");
    signal(SIGUSR1, SIG_DFL);
    signal(SIGUSR2, SIG_DFL);
}

static void nested_handler(int sig) {
    (void)sig;
    depth++;
    if (depth > max_depth) max_depth = depth;
    if (hits++ < 1) kill(getpid(), SIGUSR1);   // the same signal, from inside its own handler
    depth--;
}

static void test_nodefer(void) {
    printf("SA_NODEFER\n");
    struct sigaction sa = {0};
    sa.sa_handler = nested_handler;
    sigaction(SIGUSR1, &sa, NULL);
    hits = depth = max_depth = 0;
    kill(getpid(), SIGUSR1);
    CHECK(max_depth == 1 && hits == 2, "without: the second delivery waits (depth %d, hits %d)", max_depth, hits);
    sa.sa_flags = SA_NODEFER;
    sigaction(SIGUSR1, &sa, NULL);
    hits = depth = max_depth = 0;
    kill(getpid(), SIGUSR1);
    CHECK(max_depth == 2 && hits == 2, "with: it nests (depth %d, hits %d)", max_depth, hits);
    signal(SIGUSR1, SIG_DFL);
}

static void test_resethand(void) {
    printf("SA_RESETHAND\n");
    struct sigaction sa = {0};
    sa.sa_handler = usr2_handler;
    sa.sa_flags = SA_RESETHAND;
    sigaction(SIGUSR2, &sa, NULL);
    order_n = 0;
    kill(getpid(), SIGUSR2);
    CHECK(order_n == 1, "the handler ran once");
    struct sigaction old;
    sigaction(SIGUSR2, NULL, &old);
    CHECK(old.sa_handler == SIG_DFL, "and the disposition is SIG_DFL again");
    pid_t p = fork();
    if (p == 0) {
        kill(getpid(), SIGUSR2);           // default action: terminate
        _exit(0);
    }
    int st = 0;
    waitpid(p, &st, 0);
    CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGUSR2, "a second signal terminates (status %#x)", st);
}

static char altstack_mem[16384] __attribute__((aligned(16)));
static volatile long alt_local;
static volatile int alt_flags_inside, alt_eperm;
static void alt_handler(int sig) {
    (void)sig;
    char local;
    alt_local = (long)&local;
    stack_t cur;
    sigaltstack(NULL, &cur);
    alt_flags_inside = cur.ss_flags;
    stack_t other = { .ss_sp = altstack_mem, .ss_size = sizeof altstack_mem, .ss_flags = 0 };
    alt_eperm = sigaltstack(&other, NULL);   // not while running on it
}

static void test_altstack(void) {
    printf("sigaltstack + SA_ONSTACK\n");
    stack_t ss = { .ss_sp = altstack_mem, .ss_size = sizeof altstack_mem, .ss_flags = 0 };
    CHECK(sigaltstack(&ss, NULL) == 0, "set");
    stack_t old;
    CHECK(sigaltstack(NULL, &old) == 0 && old.ss_sp == altstack_mem && old.ss_size == sizeof altstack_mem && old.ss_flags == 0, "read back: sp %p size %zu flags %d", old.ss_sp, old.ss_size, old.ss_flags);

    struct sigaction sa = {0};
    sa.sa_handler = alt_handler;
    sa.sa_flags = SA_ONSTACK;
    sigaction(SIGUSR1, &sa, NULL);
    alt_local = 0;
    kill(getpid(), SIGUSR1);
    CHECK(alt_local >= (long)altstack_mem && alt_local < (long)(altstack_mem + sizeof altstack_mem), "handler ran on the alternate stack (%#lx)", (long)alt_local);
    CHECK(alt_flags_inside == SS_ONSTACK, "SS_ONSTACK inside the handler (%d)", alt_flags_inside);
    CHECK(alt_eperm == -1, "changing it while on it fails (%d)", alt_eperm);

    sa.sa_flags = 0;                       // no SA_ONSTACK: the ordinary stack
    sigaction(SIGUSR1, &sa, NULL);
    alt_local = 0;
    kill(getpid(), SIGUSR1);
    CHECK(alt_local != 0 && !(alt_local >= (long)altstack_mem && alt_local < (long)(altstack_mem + sizeof altstack_mem)), "without SA_ONSTACK it did not use it");

    stack_t small = { .ss_sp = altstack_mem, .ss_size = 100, .ss_flags = 0 };
    CHECK(sigaltstack(&small, NULL) == -1, "a stack under MINSIGSTKSZ is refused");
    stack_t off = { .ss_flags = SS_DISABLE };
    CHECK(sigaltstack(&off, NULL) == 0 && sigaltstack(NULL, &old) == 0 && old.ss_flags == SS_DISABLE, "SS_DISABLE");
    sa.sa_flags = SA_ONSTACK;               // asked for, but there is none now: the ordinary stack
    sigaction(SIGUSR1, &sa, NULL);
    alt_local = 0;
    kill(getpid(), SIGUSR1);
    CHECK(alt_local != 0 && !(alt_local >= (long)altstack_mem && alt_local < (long)(altstack_mem + sizeof altstack_mem)), "SA_ONSTACK with no stack set uses the ordinary one");
    signal(SIGUSR1, SIG_DFL);
}

static pid_t main_tid;
static void *sender(void *arg) {
    (void)arg;
    usleep(30000);
    sc(234 /* tgkill */, getpid(), main_tid, SIGUSR1);
    return NULL;
}

static void edit_handler(int sig, siginfo_t *info, void *ctx) {
    (void)sig; (void)info;
    ucontext_t *uc = ctx;
    uc->uc_mcontext.gregs[REG_RAX] = 1234;     // what the interrupted spin loop is waiting for
}

static void test_edit_context(void) {
    printf("editing the ucontext\n");
    struct sigaction sa = {0};
    sa.sa_sigaction = edit_handler;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGUSR1, &sa, NULL);
    main_tid = (pid_t)sc(186, 0, 0, 0);
    pthread_t t;
    pthread_create(&t, NULL, sender, NULL);
    long rax;
    // spins until the handler puts 1234 in rax; the signal lands at some instruction of this loop
    __asm__ volatile("xor %%eax, %%eax\n1: cmp $1234, %%rax\n jne 1b\n mov %%rax, %0" : "=r"(rax) : : "rax", "cc");
    pthread_join(t, NULL);
    CHECK(rax == 1234, "the process resumed with the edited register (%ld)", rax);
    signal(SIGUSR1, SIG_DFL);
}

// A restorer of the program's own, through the raw syscall: the handler must return to *it*, and sigreturn from there.
volatile long restorer_hits;
__asm__(".text\n.globl my_restorer\nmy_restorer:\n  incq restorer_hits(%rip)\n  mov $15, %rax\n  syscall\n  ud2\n");
extern void my_restorer(void);

static void test_restorer(void) {
    printf("sa_restorer\n");
    struct { void *handler; unsigned long flags; void *restorer; unsigned long mask; } ka = {
        (void *)usr2_handler, 0x04000000 /* SA_RESTORER */, (void *)my_restorer, 0 };
    register long r10 __asm__("r10") = 8;
    long ret;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(13L), "D"((long)SIGUSR2), "S"((long)&ka), "d"(0L), "r"(r10) : "rcx", "r11", "memory");
    CHECK(ret == 0, "rt_sigaction");
    order_n = 0;
    restorer_hits = 0;
    kill(getpid(), SIGUSR2);
    CHECK(order_n == 1 && restorer_hits == 1, "handler ran (%d) and returned through the restorer (%ld)", order_n, (long)restorer_hits);
    signal(SIGUSR2, SIG_DFL);
}

int main(void) {
    test_siginfo();
    test_restorer();
    test_sa_mask();
    test_nodefer();
    test_resethand();
    test_altstack();
    test_edit_context();
    if (failures) {
        printf("sigabi_test: %d FAILED\n", failures);
        return 1;
    }
    printf("sigabi_test: OK\n");
    return 0;
}
