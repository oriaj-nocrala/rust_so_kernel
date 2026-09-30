// clone(CLONE_VM) without CLONE_THREAD: a child that shares the parent's memory but is its own process — own pid and thread group,
// waited for by the parent, killed alone by exit_group or a fatal signal. And the vfork idiom (the child's writes to the parent's
// stack are seen once it execs or exits). CLONE_FILES shares the fd table; without it the child has a copy.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <fcntl.h>
#include <errno.h>
#include <time.h>
#include <sys/wait.h>

static int failures;
#define CHECK(cond, ...) do { if (!(cond)) { failures++; printf("  FAIL %s (line %d): ", #cond, __LINE__); printf(__VA_ARGS__); printf("\n"); } } while (0)

static long sc(long nr, long a, long b, long c, long d) {
    long r;
    register long r10 __asm__("r10") = d;
    __asm__ volatile("syscall" : "=a"(r) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10) : "rcx", "r11", "memory");
    return r;
}
#define CLONE_VM 0x100
#define CLONE_FILES 0x400
#define CLONE_VFORK 0x4000

// test_clone(flags, stack): clone(2); the child starts with rsp = stack holding [fn][arg], calls fn(arg) and then exit(0).
__asm__(".text\n.globl vm_clone\nvm_clone:\n"
        "  mov $56, %eax\n  xor %edx, %edx\n  xor %r10d, %r10d\n  syscall\n  test %rax, %rax\n  jnz 1f\n"
        "  pop %rax\n  pop %rdi\n  call *%rax\n  mov $60, %eax\n  xor %edi, %edi\n  syscall\n1: ret\n");
extern long vm_clone(unsigned long flags, void *stack);

static char stacks[6][16384] __attribute__((aligned(16)));
static void *prepare(int n, void (*fn)(void *), void *arg) {
    void **sp = (void **)(stacks[n] + sizeof stacks[n] - 32);
    sp[0] = (void *)fn;
    sp[1] = arg;
    return sp;
}
static void ms(int n) { struct timespec ts = {n / 1000, (n % 1000) * 1000000L}; nanosleep(&ts, NULL); }

// ── A: shared memory, separate process ───────────────────────────────────────
static volatile long c_pid, c_tid, c_ppid, c_seen_parent_write, c_wrote;
static volatile int parent_go;
static void child_a(void *arg) {
    (void)arg;
    c_pid = getpid();
    c_tid = sc(186, 0, 0, 0, 0);
    c_ppid = getppid();
    c_wrote = 1234;                                  // the parent must see this without any copy
    while (!parent_go) ms(2);                        // ...and this child must see the parent's write
    c_seen_parent_write = parent_go;
    sc(60, 7, 0, 0, 0);                              // exit(7): only this process
}

static void test_shared_process(void) {
    printf("CLONE_VM child: shared memory, own process\n");
    long me = getpid();
    long pid = vm_clone(CLONE_VM | SIGCHLD, prepare(0, child_a, 0));
    CHECK(pid > 0 && pid != me, "clone returned %ld", pid);
    for (int i = 0; i < 300 && !c_wrote; i++) ms(5);
    CHECK(c_wrote == 1234, "the parent sees the child's write to a global (%ld)", (long)c_wrote);
    CHECK(c_pid == pid && c_tid == pid, "in the child getpid()=%ld gettid()=%ld, its own pid is %ld", (long)c_pid, (long)c_tid, pid);
    CHECK(c_ppid == me, "the child's parent is us (%ld vs %ld)", (long)c_ppid, me);
    CHECK(getpid() == me, "the parent's pid did not change");
    parent_go = 42;
    int st = 0;
    CHECK(waitpid(pid, &st, 0) == pid, "waitpid finds a process child");
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 7, "status 0x%x", st);
    CHECK(c_seen_parent_write == 42, "the child saw the parent's later write (%ld)", (long)c_seen_parent_write);
}

// ── B: the child is not in the parent's thread group ─────────────────────────
static volatile long b_alive;
static void child_exit_group(void *arg) { (void)arg; sc(231, 3, 0, 0, 0); }
static void child_segv(void *arg) { (void)arg; *(volatile int *)0 = 1; }
static void child_kill_self(void *arg) { (void)arg; kill(getpid(), SIGKILL); for (;;) ; }

static void test_own_group(void) {
    printf("CLONE_VM child: exit_group and fatal signals end only the child\n");
    int st = 0;
    long pid = vm_clone(CLONE_VM | SIGCHLD, prepare(1, child_exit_group, 0));
    CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st) && WEXITSTATUS(st) == 3, "exit_group(3) in the child: status 0x%x", st);
    b_alive = 1;
    pid = vm_clone(CLONE_VM | SIGCHLD, prepare(2, child_segv, 0));
    CHECK(waitpid(pid, &st, 0) == pid && WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV, "SIGSEGV in the child: status 0x%x", st);
    pid = vm_clone(CLONE_VM | SIGCHLD, prepare(3, child_kill_self, 0));
    CHECK(waitpid(pid, &st, 0) == pid && WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL, "SIGKILL of the child: status 0x%x", st);
    CHECK(b_alive == 1, "the parent is still here");
}

// ── C: the vfork idiom ───────────────────────────────────────────────────────
static __attribute__((noinline)) void vfork_idiom(void) {
    printf("vfork: the child's writes to the parent's stack and globals are visible\n");
    volatile int local = 0;
    static volatile int global;
    global = 0;
    long pid = sc(58, 0, 0, 0, 0);
    if (pid == 0) {
        local = 42;
        global = 43;
        sc(60, 0, 0, 0, 0);
    }
    CHECK(pid > 0, "vfork returned %ld", pid);
    CHECK(local == 42 && global == 43, "local %d global %d (a copy would leave 0 0)", local, global);
    int st;
    CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st), "child reaped (0x%x)", st);
}

// ── D: exec in the child leaves the parent's memory alone ────────────────────
static char big[1 << 20];
static volatile long d_mark;
static void child_exec(void *arg) {
    (void)arg;
    d_mark = 99;
    memset(big, 0x5a, 4096);                         // shared: the parent's buffer
    char *argv[] = {"busybox", "true", NULL};
    char *envp[] = {NULL};
    execve("/bin/busybox", argv, envp);
    sc(60, 99, 0, 0, 0);
}

static void test_exec_leaves_parent(void) {
    printf("CLONE_VM|CLONE_VFORK child that execs\n");
    for (size_t i = 0; i < sizeof big; i += 4096) big[i] = (char)(i >> 12);
    d_mark = 0;
    long pid = vm_clone(CLONE_VM | CLONE_VFORK | SIGCHLD, prepare(4, child_exec, 0));
    CHECK(pid > 0, "clone returned %ld", pid);
    CHECK(d_mark == 99, "the parent, released by the exec, sees the child's write (%ld)", (long)d_mark);
    int ok = big[0] == 0x5a;                         // the child wrote here before exec
    for (size_t i = 4096; i < sizeof big; i += 4096) ok &= big[i] == (char)(i >> 12);
    CHECK(ok, "the rest of the parent's memory is intact after the child's exec");
    int st = 0;
    CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st) && WEXITSTATUS(st) == 0, "the exec'd program ran (0x%x)", st);
    big[8192] = 1;                                   // the parent can still touch its memory
}

// ── E: CLONE_FILES ───────────────────────────────────────────────────────────
static volatile int e_fd;
static void child_close(void *arg) { (void)arg; close(e_fd); }

static void test_files(void) {
    printf("CLONE_FILES: one fd table; without it a copy\n");
    int st;
    e_fd = open("/bin/busybox", O_RDONLY);
    CHECK(e_fd >= 0, "open");
    long pid = vm_clone(CLONE_VM | SIGCHLD, prepare(5, child_close, 0));
    waitpid(pid, &st, 0);
    CHECK(fcntl(e_fd, F_GETFD) >= 0, "without CLONE_FILES the child closed its own copy only");
    pid = vm_clone(CLONE_VM | CLONE_FILES | SIGCHLD, prepare(5, child_close, 0));
    waitpid(pid, &st, 0);
    CHECK(fcntl(e_fd, F_GETFD) == -1 && errno == EBADF, "with CLONE_FILES the child's close closed ours too");
}

int main(void) {
    printf("vmshare_test:\n");
    test_shared_process();
    test_own_group();
    vfork_idiom();
    test_exec_leaves_parent();
    test_files();
    printf(failures ? "vmshare_test: %d FAILURES\n" : "vmshare_test: OK\n", failures);
    return failures ? 1 : 0;
}
