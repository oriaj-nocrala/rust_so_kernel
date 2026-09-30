// Hardware faults of user code reach signal handlers: SIGSEGV (page fault, #GP), SIGILL (#UD), SIGFPE (#DE), with the right si_code
// and si_addr, resumable by editing the ucontext, on the alternate stack for a stack overflow. Without a handler, or when the fault
// repeats inside the handler (SIGSEGV is blocked there), the process dies of the right signal.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <ucontext.h>
#include <sys/mman.h>
#include <sys/wait.h>

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

static volatile int seen_sig, seen_code, hits;
static volatile void *seen_addr;
static void *resume_addr;
static volatile long skip_len;

static void fault_handler(int sig, siginfo_t *info, void *ctx) {
    ucontext_t *uc = ctx;
    seen_sig = sig;
    seen_code = info->si_code;
    seen_addr = info->si_addr;
    hits++;
    if (resume_addr) uc->uc_mcontext.gregs[REG_RIP] = (greg_t)resume_addr;
    else uc->uc_mcontext.gregs[REG_RIP] += skip_len;
}

static void install(int sig) {
    struct sigaction sa = {0};
    sa.sa_sigaction = fault_handler;
    sa.sa_flags = SA_SIGINFO | SA_NODEFER;     // NODEFER: a fault handler that edits RIP and returns needs no re-arm
    sigaction(sig, &sa, NULL);
}

static void reset(void) { seen_sig = seen_code = hits = 0; seen_addr = NULL; }

static void test_null_write(void) {
    printf("SIGSEGV: unmapped address\n");
    install(SIGSEGV);
    reset();
    __asm__ volatile("lea 1f(%%rip), %%rax\n mov %%rax, %0\n movl $1, 0x10\n 1:" : "=m"(resume_addr) :: "rax", "memory");
    CHECK(hits == 1, "the handler ran %d times", hits);
    CHECK(seen_sig == SIGSEGV, "si_signo %d", seen_sig);
    CHECK(seen_code == SEGV_MAPERR, "si_code %d, wanted SEGV_MAPERR", seen_code);
    CHECK((long)seen_addr == 0x10, "si_addr %p", (void *)seen_addr);
    resume_addr = NULL;
}

static void test_protection(void) {
    printf("SIGSEGV: write to a read-only page\n");
    char *p = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    (void)*(volatile char *)p;                  // map it (the zero frame) so the write is a protection fault
    reset();
    __asm__ volatile("lea 1f(%%rip), %%rax\n mov %%rax, %0\n movb $1, (%1)\n 1:" : "=m"(resume_addr) : "r"(p) : "rax", "memory");
    CHECK(hits == 1 && seen_sig == SIGSEGV, "handler ran %d times for signal %d", hits, seen_sig);
    CHECK(seen_code == SEGV_ACCERR, "si_code %d, wanted SEGV_ACCERR", seen_code);
    CHECK(seen_addr == p, "si_addr %p, wanted %p", (void *)seen_addr, (void *)p);

    printf("SIGSEGV: PROT_NONE\n");
    char *q = mmap(NULL, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    reset();
    __asm__ volatile("lea 1f(%%rip), %%rax\n mov %%rax, %0\n movb (%1), %%al\n 1:" : "=m"(resume_addr) : "r"(q) : "rax", "memory");
    CHECK(hits == 1 && seen_addr == q, "handler ran %d times, si_addr %p wanted %p", hits, (void *)seen_addr, (void *)q);
    resume_addr = NULL;
}

static void test_ud_and_div(void) {
    printf("SIGILL, SIGFPE, #GP\n");
    install(SIGILL);
    install(SIGFPE);
    reset();
    skip_len = 2;
    __asm__ volatile("ud2");
    CHECK(hits == 1 && seen_sig == SIGILL, "ud2: handler ran %d times for signal %d", hits, seen_sig);
    CHECK(seen_code == 2 /* ILL_ILLOPN */ || seen_code == 1, "ud2: si_code %d", seen_code);

    reset();
    __asm__ volatile("lea 1f(%%rip), %%rax\n mov %%rax, %0\n xor %%edx, %%edx\n mov $1, %%eax\n xor %%ecx, %%ecx\n div %%ecx\n 1:"
                     : "=m"(resume_addr) :: "rax", "rcx", "rdx", "cc", "memory");
    CHECK(hits == 1 && seen_sig == SIGFPE, "div by zero: handler ran %d times for signal %d", hits, seen_sig);
    CHECK(seen_code == 1 /* FPE_INTDIV */, "div by zero: si_code %d", seen_code);
    resume_addr = NULL;

    reset();
    skip_len = 1;                                // hlt is one byte and privileged: #GP
    __asm__ volatile("hlt");
    CHECK(hits == 1 && seen_sig == SIGSEGV, "hlt: handler ran %d times for signal %d", hits, seen_sig);
    CHECK(seen_code == 0x80 /* SI_KERNEL */, "hlt: si_code %#x", seen_code);
    signal(SIGSEGV, SIG_DFL);
    signal(SIGILL, SIG_DFL);
    signal(SIGFPE, SIG_DFL);
}

static void die_by(const char *what, void (*fn)(void), int want_sig) {
    pid_t p = fork();
    if (p == 0) { fn(); _exit(0); }
    int st = 0;
    waitpid(p, &st, 0);
    CHECK(WIFSIGNALED(st) && WTERMSIG(st) == want_sig, "%s: status %#x, wanted signal %d", what, st, want_sig);
}
static void do_null(void) { *(volatile int *)0x10 = 1; }
static void do_ud2(void) { __asm__ volatile("ud2"); }
// asm: the compiler may turn a C `1 / z` into a select, since z == 0 is undefined behaviour
static void do_div(void) { __asm__ volatile("xor %%edx, %%edx\n mov $1, %%eax\n xor %%ecx, %%ecx\n div %%ecx" ::: "rax", "rcx", "rdx", "cc"); }

static void test_default_kills(void) {
    printf("no handler: the right signal kills\n");
    die_by("null write", do_null, SIGSEGV);
    die_by("ud2", do_ud2, SIGILL);
    die_by("divide by zero", do_div, SIGFPE);
}

static volatile int refault_depth;
static void refault_handler(int sig, siginfo_t *info, void *ctx) {
    (void)sig; (void)info; (void)ctx;
    if (++refault_depth > 1) _exit(77);          // only reached if SIGSEGV was delivered again, inside its own handler
    *(volatile int *)0x20 = 1;                   // SIGSEGV is blocked inside its own handler: this kills
}

static void test_refault(void) {
    printf("a fault inside the SIGSEGV handler\n");
    pid_t p = fork();
    if (p == 0) {
        struct sigaction sa = {0};
        sa.sa_sigaction = refault_handler;
        sa.sa_flags = SA_SIGINFO;
        sigaction(SIGSEGV, &sa, NULL);
        *(volatile int *)0x10 = 1;
        _exit(0);
    }
    int st = 0;
    waitpid(p, &st, 0);
    CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV, "the process dies of SIGSEGV (status %#x)", st);
}

static char altstack_mem[65536] __attribute__((aligned(16)));

static void overflow_handler(int sig, siginfo_t *info, void *ctx) {
    (void)sig; (void)ctx;
    // the fault address is just below the stack; the handler runs on the alternate stack
    char here;
    int on_alt = (char *)&here >= altstack_mem && (char *)&here < altstack_mem + sizeof altstack_mem;
    _exit(on_alt && info->si_addr != NULL ? 42 : 43);
}

static int recurse(int n) {
    volatile char pad[4096];
    pad[0] = (char)n;
    return recurse(n + 1) + pad[0];
}

static void test_stack_overflow(void) {
    printf("stack overflow\n");
    pid_t p = fork();
    if (p == 0) {
        recurse(0);                              // no handler: killed
        _exit(0);
    }
    int st = 0;
    waitpid(p, &st, 0);
    CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV, "without a handler: status %#x", st);

    p = fork();
    if (p == 0) {
        struct sigaction sa = {0};
        sa.sa_sigaction = overflow_handler;
        sa.sa_flags = SA_SIGINFO;                // a handler but no SA_ONSTACK: there is no room for its frame
        sigaction(SIGSEGV, &sa, NULL);
        recurse(0);
        _exit(0);
    }
    waitpid(p, &st, 0);
    CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV, "a handler on the overflowed stack cannot run: killed (status %#x)", st);

    p = fork();
    if (p == 0) {
        stack_t ss = { .ss_sp = altstack_mem, .ss_size = sizeof altstack_mem };
        sigaltstack(&ss, NULL);
        struct sigaction sa = {0};
        sa.sa_sigaction = overflow_handler;
        sa.sa_flags = SA_SIGINFO | SA_ONSTACK;
        sigaction(SIGSEGV, &sa, NULL);
        recurse(0);
        _exit(0);
    }
    waitpid(p, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 42, "with SA_ONSTACK the handler runs on the alternate stack (status %#x)", st);
}

int main(void) {
    test_null_write();
    test_protection();
    test_ud_and_div();
    test_default_kills();
    test_refault();
    test_stack_overflow();
    if (failures) {
        printf("sigsegv_test: %d FAILED\n", failures);
        return 1;
    }
    printf("sigsegv_test: OK\n");
    return 0;
}
