// clone(CLONE_VFORK): the parent sleeps until the child execs or dies, and gets the child's pid. Without the flag it returns at once.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <signal.h>
#include <time.h>
#include <sys/wait.h>

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

static long sc(long nr, long a, long b, long c, long d) {
    long r;
    register long r10 __asm__("r10") = d;
    __asm__ volatile("syscall" : "=a"(r) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10) : "rcx", "r11", "memory");
    return r;
}
#define CLONE_VM 0x100
#define CLONE_VFORK 0x4000
enum { WAIT_MS = 0, EXEC_SLEEP = 1, KILLED = 2 };

static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }
static void ms(int n) { struct timespec ts = {n / 1000, (n % 1000) * 1000000L}; nanosleep(&ts, NULL); }

// clone(flags | SIGCHLD); the child does `how` and never returns. Returns the parent's view: pid, and how long clone took.
static long spawn(long flags, int how, double *took) {
    double t0 = now();
    long pid = sc(56, flags | SIGCHLD, 0, 0, 0);
    if (pid == 0) {
        if (how == WAIT_MS) { ms(300); _exit(5); }
        if (how == EXEC_SLEEP) {
            ms(100);
            char *argv[] = {"busybox", "sleep", "1", NULL};
            char *envp[] = {NULL};
            execve("/bin/busybox", argv, envp);
            _exit(99);
        }
        if (how == KILLED) { ms(200); kill(getpid(), SIGKILL); for (;;) ; }
    }
    *took = now() - t0;
    return pid;
}

int main(void) {
    double took;
    int st;

    printf("CLONE_VFORK: the parent waits for the child's exit\n");
    long pid = spawn(CLONE_VM | CLONE_VFORK, WAIT_MS, &took);
    CHECK(pid > 0, "clone returned %ld", pid);
    CHECK(took >= 0.28 && took < 0.9, "clone took %.3f s, wanted about 0.3 (the child's lifetime)", took);
    CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st) && WEXITSTATUS(st) == 5, "waitpid: the child's status 0x%x", st);

    printf("CLONE_VFORK: the parent is released by exec, not by exit\n");
    pid = spawn(CLONE_VM | CLONE_VFORK, EXEC_SLEEP, &took);
    CHECK(pid > 0, "clone returned %ld", pid);
    CHECK(took >= 0.08 && took < 0.7, "clone took %.3f s, wanted about 0.1 (exec) and well under the exec'd sleep of 1 s", took);
    CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st) && WEXITSTATUS(st) == 0, "the exec'd child's status 0x%x", st);

    printf("CLONE_VFORK: the parent is released when the child is killed\n");
    pid = spawn(CLONE_VM | CLONE_VFORK, KILLED, &took);
    CHECK(pid > 0 && took >= 0.18 && took < 0.9, "clone took %.3f s (pid %ld), wanted about 0.2", took, pid);
    CHECK(waitpid(pid, &st, 0) == pid && WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL, "status 0x%x", st);

    printf("without CLONE_VFORK the parent does not wait\n");
    pid = spawn(0, WAIT_MS, &took);
    CHECK(pid > 0 && took < 0.15, "clone took %.3f s (pid %ld), wanted about 0", took, pid);
    st = -1;
    CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st) && WEXITSTATUS(st) == 5, "a waitpid that blocks got status 0x%x, wanted exit(5)", st);

    printf("a plain fork() is unaffected\n");
    double t0 = now();
    pid = fork();
    if (pid == 0) { ms(100); _exit(0); }
    CHECK(now() - t0 < 0.08, "fork took %.3f s", now() - t0);
    waitpid(pid, &st, 0);

    printf(failures ? "vfork_test: FAIL\n" : "vfork_test: PASS\n");
    return failures != 0;
}
