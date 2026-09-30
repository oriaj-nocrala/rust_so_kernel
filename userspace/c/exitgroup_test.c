// exit_group from a spawned thread: the process's parent must see exit(status), whichever thread ends it and whatever the main
// thread is doing (spinning, blocked in pthread_join). A fatal signal in a thread still reports the signal, not an exit.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <pthread.h>
#include <sys/wait.h>

static void exit_group(int status) {
    __asm__ volatile("syscall" :: "a"(231L), "D"((long)status) : "rcx", "r11", "memory");
    for (;;) ;
}

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

enum { RAW_EXIT_GROUP, LIBC_EXIT, SEGV, MAIN_EXIT_GROUP };
static int code;

static void *thread_main(void *arg) {
    long how = (long)arg;
    usleep(20000);                                   // let the main thread reach its spin / join first
    if (how == RAW_EXIT_GROUP) exit_group(code);
    if (how == LIBC_EXIT) exit(code);
    if (how == SEGV) *(volatile int *)0x10 = 1;
    for (;;) usleep(10000);                          // MAIN_EXIT_GROUP: the main thread ends the process
}

static int run(long how, int main_joins, int *status) {
    pid_t pid = fork();
    if (pid == 0) {
        pthread_t t;
        pthread_create(&t, NULL, thread_main, (void *)how);
        if (how == MAIN_EXIT_GROUP) { usleep(60000); exit_group(code); }
        if (main_joins) pthread_join(t, NULL);
        for (;;) __asm__ volatile("pause");
    }
    return waitpid(pid, status, 0) == pid;
}

int main(void) {
    int st;
    printf("thread calls exit_group, main spins\n");
    code = 42;
    CHECK(run(RAW_EXIT_GROUP, 0, &st), "waitpid failed");
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 42, "status 0x%x", st);

    printf("thread calls exit_group, main blocked in pthread_join\n");
    code = 43;
    CHECK(run(RAW_EXIT_GROUP, 1, &st), "waitpid failed");
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 43, "status 0x%x", st);

    printf("thread calls libc exit()\n");
    code = 45;
    CHECK(run(LIBC_EXIT, 0, &st), "waitpid failed");
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 45, "status 0x%x", st);

    printf("main thread calls exit_group, a thread spinning\n");
    code = 44;
    CHECK(run(MAIN_EXIT_GROUP, 0, &st), "waitpid failed");
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 44, "status 0x%x", st);

    printf("thread takes SIGSEGV: the signal is reported, not an exit\n");
    CHECK(run(SEGV, 0, &st), "waitpid failed");
    CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV, "status 0x%x", st);

    printf(failures ? "exitgroup_test: FAIL\n" : "exitgroup_test: PASS\n");
    return failures != 0;
}
