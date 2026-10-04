// userspace/c/itimer_test.c
//
// ITIMER_REAL through mlibc: alarm(), setitimer()/getitimer(), SIGALRM
// delivery to a handler, a periodic timer, interruption of a blocking call,
// the default action (terminate), and non-inheritance across fork.
// Prints one line per check, then PASS/FAIL.

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures = 0;
static volatile int alarms = 0;

static void check(int ok, const char *what) {
    printf("%s %s\n", ok ? "  ok  " : "  FAIL", what);
    if (!ok) failures++;
}

static void on_alarm(int sig) {
    (void)sig;
    alarms++;
}

static long long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_alarm;
    sigaction(SIGALRM, &sa, 0);

    /* alarm(): the handler runs once, about a second later; pause() returns EINTR. */
    long long t0 = now_ms();
    check(alarm(1) == 0, "alarm(1) with nothing pending returns 0");
    errno = 0;
    int r = pause();
    long long dt = now_ms() - t0;
    check(r == -1 && errno == EINTR && alarms == 1, "pause() is interrupted by SIGALRM, handler ran once");
    check(dt >= 900 && dt < 1600, "the alarm fired after about 1 s");

    /* A pending alarm is replaced, and alarm() reports the seconds left (rounded up). */
    alarm(10);
    unsigned left = alarm(0);
    check(left >= 9 && left <= 10, "alarm(0) cancels and reports the time left");
    check(alarm(0) == 0, "nothing left after cancelling");
    int before = alarms;
    usleep(300 * 1000);
    check(alarms == before, "a cancelled alarm never fires");

    /* setitimer: periodic every 100 ms, first after 100 ms. */
    struct itimerval it = { .it_interval = { 0, 100000 }, .it_value = { 0, 100000 } };
    struct itimerval old;
    alarms = 0;
    check(setitimer(ITIMER_REAL, &it, &old) == 0 && old.it_value.tv_sec == 0 && old.it_value.tv_usec == 0, "setitimer arms it; the old value was empty");
    struct itimerval cur;
    check(getitimer(ITIMER_REAL, &cur) == 0 && cur.it_interval.tv_usec == 100000 && cur.it_value.tv_usec <= 100000, "getitimer reports the interval and the time left");
    t0 = now_ms();
    while (now_ms() - t0 < 1050) {
        struct timespec ts = { 0, 20 * 1000 * 1000 };
        nanosleep(&ts, 0);
    }
    check(alarms >= 8 && alarms <= 11, "a 100 ms periodic timer ticked about 10 times in 1 s");
    memset(&it, 0, sizeof it);
    check(setitimer(ITIMER_REAL, &it, &old) == 0 && old.it_interval.tv_usec == 100000, "disarming returns the previous interval");
    before = alarms;
    usleep(300 * 1000);
    check(alarms == before, "no more ticks once disarmed");

    errno = 0;
    check(setitimer(ITIMER_VIRTUAL, &it, 0) < 0 && errno == EINVAL, "ITIMER_VIRTUAL is not supported (EINVAL)");
    struct itimerval bad = { .it_value = { 0, 2000000 } };
    errno = 0;
    check(setitimer(ITIMER_REAL, &bad, 0) < 0 && errno == EINVAL, "tv_usec out of range -> EINVAL");

    /* Not inherited by a child: arm here, the child must see an empty timer. */
    alarm(30);
    pid_t child = fork();
    if (child == 0) {
        struct itimerval c;
        getitimer(ITIMER_REAL, &c);
        _exit(c.it_value.tv_sec == 0 && c.it_value.tv_usec == 0 ? 0 : 1);
    }
    int st = 0;
    waitpid(child, &st, 0);
    check(WIFEXITED(st) && WEXITSTATUS(st) == 0, "a forked child starts with no timer");
    alarm(0);

    /* The default action kills the process. */
    child = fork();
    if (child == 0) {
        signal(SIGALRM, SIG_DFL);
        alarm(1);
        for (;;) pause();
    }
    waitpid(child, &st, 0);
    check(WIFSIGNALED(st) && WTERMSIG(st) == SIGALRM, "without a handler SIGALRM terminates the process");

    printf(failures ? "itimer_test: FAIL (%d)\n" : "itimer_test: PASS\n", failures);
    return failures != 0;
}
