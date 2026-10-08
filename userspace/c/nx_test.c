// NX on user pages: code copied into memory without PROT_EXEC (an anonymous mapping, a page only ever read, so still the
// shared zero frame, the stack, .data) faults with SIGSEGV/SEGV_ACCERR whose si_addr and rip are the code's own address;
// mprotect(PROT_EXEC) or mmap(PROT_EXEC) makes it run; fork keeps both kinds of page; /proc/self/maps shows the x bit; a
// handler that returns still goes through the (executable) signal trampoline.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <signal.h>
#include <ucontext.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/wait.h>

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

// mov eax, 42; ret
static const uint8_t CODE[] = { 0xb8, 0x2a, 0x00, 0x00, 0x00, 0xc3 };
typedef int (*fn_t)(void);

// mlibc's mprotect has no sysdep: the raw syscall.
static long mprotect_(void *addr, size_t len, int prot) {
    long ret;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(10), "D"(addr), "S"(len), "d"(prot) : "rcx", "r11", "memory");
    return ret;
}

static volatile uintptr_t expect;

// The fault we want: an instruction fetch at `expect` (rip and si_addr both there, SEGV_ACCERR). Anything else, a
// different exit code, so a wrong kind of fault is visible as such.
static void on_segv(int sig, siginfo_t *info, void *ctx) {
    ucontext_t *uc = ctx;
    uintptr_t rip = (uintptr_t)uc->uc_mcontext.gregs[REG_RIP];
    if (sig == SIGSEGV && info->si_code == SEGV_ACCERR && (uintptr_t)info->si_addr == expect && rip == expect) _exit(77);
    _exit(78);
}

// Run `f` in a child with the handler above; return the child's exit code (or 128 + signal). `touch_first`: the child
// reads `f` before calling it, so the page's PTE is made in the child (fork rebuilds the child's PTEs from the VMA flags,
// which would hide a wrong PTE made in the parent).
static int run_child_ex(void *f, int with_handler, int touch_first) {
    pid_t pid = fork();
    if (pid == 0) {
        if (touch_first) { volatile uint8_t sink = *(volatile uint8_t *)f; (void)sink; }
        if (with_handler) {
            struct sigaction sa = {0};
            sa.sa_sigaction = on_segv;
            sa.sa_flags = SA_SIGINFO;
            sigaction(SIGSEGV, &sa, NULL);
        }
        expect = (uintptr_t)f;
        int r = ((fn_t)f)();
        _exit(r == 42 ? 42 : 43);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    return WIFEXITED(st) ? WEXITSTATUS(st) : WIFSIGNALED(st) ? 128 + WTERMSIG(st) : -1;
}
static int run_child(void *f, int with_handler) { return run_child_ex(f, with_handler, 0); }

static uint8_t data_code[64] __attribute__((aligned(16)));

static char maps_perms[8];
static const char *perms_of(void *addr) {
    static char buf[8192];
    FILE *f = fopen("/proc/self/maps", "r");
    maps_perms[0] = 0;
    if (!f) return maps_perms;
    size_t n = fread(buf, 1, sizeof buf - 1, f);
    fclose(f);
    buf[n] = 0;
    for (char *line = buf; *line; ) {
        unsigned long s, e; char p[8];
        if (sscanf(line, "%lx-%lx %4s", &s, &e, p) == 3 && (uintptr_t)addr >= s && (uintptr_t)addr < e) {
            memcpy(maps_perms, p, 5);
            break;
        }
        char *nl = strchr(line, '\n');
        if (!nl) break;
        line = nl + 1;
    }
    return maps_perms;
}

static volatile int usr1_seen;
static void on_usr1(int sig) { (void)sig; usr1_seen++; }

int main(void) {
    printf("anonymous memory\n");
    uint8_t *rw = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    memcpy(rw, CODE, sizeof CODE);
    CHECK(run_child(rw, 1) == 77, "calling into PROT_READ|PROT_WRITE: SIGSEGV at the code itself (got %d)", run_child(rw, 1));
    CHECK(run_child(rw, 0) == 128 + SIGSEGV, "without a handler: killed by SIGSEGV (got %d)", run_child(rw, 0));
    CHECK(strcmp(perms_of(rw), "rw-p") == 0, "maps says '%s'", perms_of(rw));

    CHECK(mprotect_(rw, 4096, PROT_READ | PROT_EXEC) == 0, "mprotect(PROT_READ|PROT_EXEC)");
    CHECK(((fn_t)rw)() == 42, "then it runs");
    CHECK(strcmp(perms_of(rw), "r-xp") == 0, "maps says '%s'", perms_of(rw));
    CHECK(run_child(rw, 1) == 42, "and runs in a child forked after (got %d)", run_child(rw, 1));
    CHECK(mprotect_(rw, 4096, PROT_READ) == 0, "mprotect(PROT_READ) takes exec away again");
    CHECK(run_child(rw, 1) == 77, "now it faults (got %d)", run_child(rw, 1));

    uint8_t *rwx = mmap(NULL, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    memcpy(rwx, CODE, sizeof CODE);
    CHECK(((fn_t)rwx)() == 42, "mmap(PROT_READ|PROT_WRITE|PROT_EXEC) runs");
    CHECK(strcmp(perms_of(rwx), "rwxp") == 0, "maps says '%s'", perms_of(rwx));

    // Only read, never written: the PTE is the shared zero frame, which must still be NX. Read in the child, after the fork.
    uint8_t *zero = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(run_child_ex(zero, 1, 1) == 77, "a zero-frame page: SIGSEGV at it (got %d)", run_child_ex(zero, 1, 1));

    // Huge-page path (2 MiB mappings get 2 MiB pages).
    size_t big = 4u << 20;
    uint8_t *huge = mmap(NULL, big, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    memcpy(huge + 8192, CODE, sizeof CODE);
    CHECK(run_child(huge + 8192, 1) == 77, "a 4 MiB mapping: SIGSEGV (got %d)", run_child(huge + 8192, 1));
    CHECK(mprotect_(huge, big, PROT_READ | PROT_EXEC) == 0, "mprotect the 4 MiB mapping executable");
    CHECK(((fn_t)(huge + 8192))() == 42, "then it runs");

    printf("stack and data\n");
    uint8_t stack_code[64] __attribute__((aligned(16)));
    memcpy(stack_code, CODE, sizeof CODE);
    CHECK(run_child(stack_code, 1) == 77, "code on the stack: SIGSEGV (got %d)", run_child(stack_code, 1));
    CHECK(perms_of(stack_code)[2] == '-', "the stack is not x in maps ('%s')", perms_of(stack_code));
    memcpy(data_code, CODE, sizeof CODE);
    CHECK(run_child(data_code, 1) == 77, "code in .data: SIGSEGV (got %d)", run_child(data_code, 1));
    CHECK(perms_of((void *)main)[2] == 'x', "the program's own code is x ('%s')", perms_of((void *)main));

    printf("signal trampoline\n");
    signal(SIGUSR1, on_usr1);
    raise(SIGUSR1);
    CHECK(usr1_seen == 1, "a handler that returns comes back through the trampoline");

    printf(failures ? "nx_test: FAIL\n" : "nx_test: PASS\n");
    return failures != 0;
}
