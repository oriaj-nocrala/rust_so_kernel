// Smoke test for termios/job control: tcgetattr/tcsetattr, setpgid/getpgid,
// SIGSTOP/SIGCONT + waitpid(WUNTRACED), and kill() to a process group.
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <signal.h>
#include <termios.h>
#include <sys/wait.h>
#include <time.h>

static void sleep_ms(long ms) {
    struct timespec ts = {0, ms * 1000000L};
    nanosleep(&ts, NULL);
}

int main(void) {
    printf("isatty(0)=%d isatty(1)=%d isatty(2)=%d\n", isatty(0), isatty(1), isatty(2));

    // ── termios: tcgetattr/tcsetattr round-trip ─────────────────────────
    struct termios t;
    if (tcgetattr(0, &t) != 0) {
        printf("tcgetattr FAILED\n");
        return 1;
    }
    printf("tcgetattr: c_lflag=0x%x ICANON=%d ECHO=%d ISIG=%d\n",
           t.c_lflag, !!(t.c_lflag & ICANON), !!(t.c_lflag & ECHO), !!(t.c_lflag & ISIG));

    tcflag_t original_lflag = t.c_lflag;
    t.c_lflag &= ~(ICANON | ECHO);
    if (tcsetattr(0, TCSANOW, &t) != 0) {
        printf("tcsetattr FAILED\n");
        return 1;
    }
    struct termios t2;
    if (tcgetattr(0, &t2) != 0 || (t2.c_lflag & (ICANON | ECHO)) != 0) {
        printf("tcsetattr_test FAILED: ICANON/ECHO still set after clearing (c_lflag=0x%x)\n", t2.c_lflag);
        return 1;
    }
    // Restore, since this tty is shared with the shell.
    t.c_lflag = original_lflag;
    if (tcsetattr(0, TCSANOW, &t) != 0) {
        printf("tcsetattr restore FAILED\n");
        return 1;
    }
    printf("termios_test: OK\n");

    // ── setpgid/getpgid ──────────────────────────────────────────────────
    pid_t self = getpid();
    pid_t pgid_before = getpgid(0);
    printf("getpgid(0)=%d (self=%d)\n", (int)pgid_before, (int)self);

    if (setpgid(0, 0) != 0) {
        printf("setpgid(0,0) FAILED\n");
        return 1;
    }
    pid_t pgid_after = getpgid(0);
    if (pgid_after != self) {
        printf("setpgid_test FAILED: expected own pgid==%d, got %d\n", (int)self, (int)pgid_after);
        return 1;
    }
    printf("setpgid_test: OK (pgid now %d)\n", (int)pgid_after);

    // ── SIGSTOP / waitpid(WUNTRACED) / SIGCONT ──────────────────────────
    pid_t child = fork();
    if (child < 0) {
        printf("fork FAILED\n");
        return 1;
    }
    if (child == 0) {
        kill(getpid(), SIGSTOP);
        printf("child: resumed after SIGCONT\n");
        _exit(7);
    }

    int status = -1;
    pid_t r = waitpid(child, &status, WUNTRACED);
    if (r != child || !WIFSTOPPED(status) || WSTOPSIG(status) != SIGSTOP) {
        printf("waitpid(WUNTRACED) FAILED: r=%d status=0x%x\n", (int)r, status);
        return 1;
    }
    printf("waitpid(WUNTRACED): child %d stopped by signal %d\n", (int)r, WSTOPSIG(status));

    if (kill(child, SIGCONT) != 0) {
        printf("kill(SIGCONT) FAILED\n");
        return 1;
    }

    status = -1;
    r = waitpid(child, &status, 0);
    if (r != child || !WIFEXITED(status) || WEXITSTATUS(status) != 7) {
        printf("waitpid after SIGCONT FAILED: r=%d status=0x%x\n", (int)r, status);
        return 1;
    }
    printf("stop_continue_test: OK (child exited %d after SIGCONT)\n", WEXITSTATUS(status));

    // ── kill() to a process group ────────────────────────────────────────
    pid_t c1 = fork();
    if (c1 == 0) {
        setpgid(0, 0);
        for (int i = 0; i < 50; i++) sleep_ms(20);
        _exit(99); // shouldn't get here — SIGTERM should land first
    }
    setpgid(c1, c1); // also set from the parent side (race-safe, POSIX-sanctioned)

    pid_t c2 = fork();
    if (c2 == 0) {
        setpgid(0, c1);
        for (int i = 0; i < 50; i++) sleep_ms(20);
        _exit(99);
    }
    setpgid(c2, c1);

    if (kill(-c1, SIGTERM) != 0) {
        printf("kill(-pgid, SIGTERM) FAILED\n");
        return 1;
    }

    int st1 = -1, st2 = -1;
    pid_t r1 = waitpid(c1, &st1, 0);
    pid_t r2 = waitpid(c2, &st2, 0);
    if (r1 != c1 || !WIFSIGNALED(st1) || WTERMSIG(st1) != SIGTERM) {
        printf("group_kill_test FAILED: c1 r=%d status=0x%x\n", (int)r1, st1);
        return 1;
    }
    if (r2 != c2 || !WIFSIGNALED(st2) || WTERMSIG(st2) != SIGTERM) {
        printf("group_kill_test FAILED: c2 r=%d status=0x%x\n", (int)r2, st2);
        return 1;
    }
    printf("group_kill_test: OK (both group members killed by SIGTERM)\n");

    printf("jobctl_test: OK\n");
    return 0;
}
