// waitpid(WCONTINUED): a child resumed by SIGCONT is reported once, with status 0xffff (WIFCONTINUED), both when the parent
// polls afterwards and when it is already blocked in the wait. Without the flag it is never reported.
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <signal.h>
#include <sys/wait.h>
#include <time.h>

static int failures;
#define CHECK(cond, msg) do { if (!(cond)) { printf("FAIL: %s (line %d)\n", msg, __LINE__); failures++; } } while (0)

static void sleep_ms(long ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
    nanosleep(&ts, NULL);
}

static pid_t spawn_spinner(void) {
    pid_t p = fork();
    if (p == 0) { for (;;) sleep_ms(5); }
    return p;
}

static void stop_and_reap_stop(pid_t p) {
    int st = 0;
    kill(p, SIGSTOP);
    pid_t r = waitpid(p, &st, WUNTRACED);
    CHECK(r == p && WIFSTOPPED(st) && WSTOPSIG(st) == SIGSTOP, "stop reported");
}

int main(void) {
    // 1. Polling after the resume.
    pid_t c = spawn_spinner();
    stop_and_reap_stop(c);
    int st = 0;
    CHECK(waitpid(c, &st, WCONTINUED | WNOHANG) == 0, "not continued yet: WNOHANG returns 0");
    kill(c, SIGCONT);
    sleep_ms(50);
    // Without WCONTINUED the resume is invisible.
    CHECK(waitpid(c, &st, WNOHANG | WUNTRACED) == 0, "no WCONTINUED: nothing to report");
    st = 0;
    pid_t r = waitpid(c, &st, WCONTINUED);
    CHECK(r == c, "WCONTINUED returns the child");
    CHECK(WIFCONTINUED(st), "status is WIFCONTINUED");
    CHECK(!WIFSTOPPED(st) && !WIFEXITED(st) && !WIFSIGNALED(st), "status is only 'continued'");
    CHECK(waitpid(c, &st, WCONTINUED | WNOHANG) == 0, "reported once");

    // 2. Blocked in the wait before the SIGCONT arrives (a helper process sends it later).
    stop_and_reap_stop(c);
    pid_t helper = fork();
    if (helper == 0) { sleep_ms(150); kill(c, SIGCONT); _exit(0); }
    st = 0;
    r = waitpid(c, &st, WCONTINUED);
    CHECK(r == c && WIFCONTINUED(st), "blocked waitpid(WCONTINUED) is woken by the SIGCONT");
    waitpid(helper, NULL, 0);

    // 3. Any child (-1), and a stop after the resume is reported as a stop again, not as a continue.
    stop_and_reap_stop(c);
    kill(c, SIGCONT);
    sleep_ms(30);
    st = 0;
    r = waitpid(-1, &st, WCONTINUED);
    CHECK(r == c && WIFCONTINUED(st), "waitpid(-1, WCONTINUED)");
    kill(c, SIGSTOP);
    sleep_ms(30);
    st = 0;
    r = waitpid(-1, &st, WCONTINUED | WUNTRACED);
    CHECK(r == c && WIFSTOPPED(st), "a new stop is reported as a stop");

    // 4. A stop cancels an unreported continue.
    kill(c, SIGCONT);
    sleep_ms(30);
    kill(c, SIGSTOP);
    sleep_ms(30);
    st = 0;
    r = waitpid(c, &st, WCONTINUED | WNOHANG);
    CHECK(r == 0, "stopped again before the continue was collected: nothing to report");

    kill(c, SIGKILL);
    waitpid(c, &st, 0);
    CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL, "cleanup kill");

    printf(failures ? "wcontinued_test: %d FAILURES\n" : "wcontinued_test: OK\n", failures);
    return failures ? 1 : 0;
}
