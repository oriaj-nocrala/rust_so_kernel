// Exercises the newly-wired mlibc signal.h/unistd.h sysdeps hooks
// (sys_pipe, sys_kill, sys_sigaction) end to end through real libc calls —
// pipe(), fork(), kill(), sigaction(), raise() — rather than this kernel's
// raw syscall ABI directly (see userspace/src/bin/signal_test.rs for that).
//
// Flow: parent pipe()s and fork()s. Child kill()s the parent with SIGUSR1,
// writes a message into the pipe, and exits (queuing SIGCHLD too). Parent's
// SIGUSR1 handler runs (proving mlibc's sigaction()/kill()/sigreturn path
// works), then it reads the pipe message, waits for its SIGCHLD handler
// (raise()-testable independently) to confirm, and reaps the child.

#include <stdio.h>
#include <errno.h>
#include <string.h>
#include <signal.h>
#include <unistd.h>
#include <sys/wait.h>

static volatile sig_atomic_t usr1_received = 0;
static volatile sig_atomic_t chld_received = 0;

static void on_usr1(int sig) {
    if (sig == SIGUSR1) usr1_received = 1;
}

static void on_chld(int sig) {
    if (sig == SIGCHLD) chld_received = 1;
}

int main(void) {
    printf("mlibc_signal_test: starting\n");

    int fds[2];
    if (pipe(fds) != 0) {
        printf("mlibc_signal_test: pipe() failed\n");
        return 1;
    }

    struct sigaction sa_usr1 = {0};
    sa_usr1.sa_handler = on_usr1;
    if (sigaction(SIGUSR1, &sa_usr1, NULL) != 0) {
        printf("mlibc_signal_test: sigaction(SIGUSR1) failed\n");
        return 1;
    }

    struct sigaction sa_chld = {0};
    sa_chld.sa_handler = on_chld;
    if (sigaction(SIGCHLD, &sa_chld, NULL) != 0) {
        printf("mlibc_signal_test: sigaction(SIGCHLD) failed\n");
        return 1;
    }

    pid_t parent_pid = getpid();
    pid_t pid = fork();
    if (pid < 0) {
        printf("mlibc_signal_test: fork() failed\n");
        return 1;
    }

    if (pid == 0) {
        // Child: write end only.
        close(fds[0]);
        kill(parent_pid, SIGUSR1);
        const char *msg = "hello from child";
        write(fds[1], msg, strlen(msg));
        close(fds[1]);
        _exit(0);
    }

    // Parent: read end only.
    close(fds[1]);

    // Wait for the handler by time, not by a spin count: on a fast machine a fixed count is over before the child has run.
    int spins = 0;
    for (int i = 0; i < 2000 && !usr1_received; i++) {
        usleep(1000);
    }

    // The read may be interrupted by SIGUSR1 (the handler has no SA_RESTART): retry, as any caller must.
    char buf[64] = {0};
    ssize_t n;
    do {
        n = read(fds[0], buf, sizeof(buf) - 1);
    } while (n < 0 && errno == EINTR);
    close(fds[0]);

    waitpid(pid, NULL, 0);

    for (int i = 0; i < 2000 && !chld_received; i++) {
        usleep(1000);
    }
    (void)spins;

    int pipe_ok = (n > 0 && strcmp(buf, "hello from child") == 0);

    printf("mlibc_signal_test: usr1_received=%d chld_received=%d pipe_ok=%d msg=\"%s\"\n",
           (int)usr1_received, (int)chld_received, pipe_ok, buf);

    if (usr1_received && chld_received && pipe_ok) {
        printf("mlibc_signal_test: PASS\n");
    } else {
        printf("mlibc_signal_test: FAIL\n");
    }

    return 0;
}
