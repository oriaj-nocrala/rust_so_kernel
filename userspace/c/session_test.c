// session_test: sessions (phase 3.2 of docs/gui/gui-plan.md). Until
// 2026-09-25 there was no session id: setsid() only made the caller a group
// leader, getsid() did not exist, and setpgid() moved any process into any
// group.
//
//   A. a child is in its parent's session (getsid);
//   B. setsid() from a group leader is EPERM; from a forked child it makes a
//      new session and group named after it, and a fork of that child
//      inherits the new session;
//   C. setpgid across sessions is EPERM, both ways; a session leader
//      cannot change group (EPERM);
//   D. setpgid on a process that is neither the caller nor its child is
//      ESRCH; joining a group that does not exist is EPERM; joining a
//      sibling's group in the same session works;
//   E. getsid of a pid that does not exist is ESRCH;
//   F. SIGWINCH and SIGURG are ignored by default (they used to kill);
//   G. /proc/self/stat reports the session in field 6.
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>
#include <sys/wait.h>

static int fails;

static void check(const char *what, int ok) {
    printf("  %s -> %s\n", what, ok ? "PASS" : "FAIL");
    if (!ok) fails++;
}

static void nap_ms(long ms) {
    struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L };
    nanosleep(&ts, NULL);
}

// Run `fn` in a child; its exit status is its number of failures.
static void in_child(const char *name, int (*fn)(void)) {
    printf("%s\n", name);
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        int f = fn();
        fflush(stdout);
        _exit(f);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    if (!WIFEXITED(st)) {
        printf("  child died (status %#x) -> FAIL\n", st);
        fails++;
    } else {
        fails += WEXITSTATUS(st);
    }
}

static int local_fails;
static void lcheck(const char *what, int ok) {
    printf("  %s -> %s\n", what, ok ? "PASS" : "FAIL");
    if (!ok) local_fails++;
}

static int case_setsid(void) {
    local_fails = 0;
    pid_t me = getpid();
    pid_t old_sid = getsid(0);
    // A fork of a process keeps its parent's group, so this child is not
    // a group leader and may start a session.
    pid_t sid = setsid();
    lcheck("setsid from a forked child returns its pid", sid == me);
    lcheck("getsid(0) is the new session", getsid(0) == me);
    lcheck("getpgid(0) is the new group", getpgid(0) == me);
    lcheck("the parent stays in the old session", getsid(getppid()) == old_sid);
    errno = 0;
    lcheck("setsid again (now a leader) is EPERM", setsid() == -1 && errno == EPERM);
    errno = 0;
    lcheck("a session leader cannot change group (EPERM)",
           setpgid(0, getpgid(getppid())) == -1 && errno == EPERM);

    pid_t gc = fork();
    if (gc == 0) {
        _exit(getsid(0) == me && getpgid(0) == me ? 0 : 1);
    }
    int st = 0;
    waitpid(gc, &st, 0);
    lcheck("a fork inherits the new session", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    return local_fails;
}

static int case_cross_session(void) {
    local_fails = 0;
    int to_parent[2];
    pipe(to_parent);
    pid_t child = fork();
    if (child == 0) {
        setsid();
        write(to_parent[1], "x", 1);
        pause();
        _exit(0);
    }
    char c;
    read(to_parent[0], &c, 1);
    errno = 0;
    lcheck("parent moving a child in another session is EPERM",
           setpgid(child, getpgid(0)) == -1 && errno == EPERM);
    errno = 0;
    lcheck("joining a group of another session is EPERM",
           setpgid(0, child) == -1 && errno == EPERM);
    kill(child, SIGKILL);
    waitpid(child, NULL, 0);
    return local_fails;
}

static int case_groups(void) {
    local_fails = 0;
    errno = 0;
    lcheck("setpgid on the parent (not a child) is ESRCH",
           setpgid(getppid(), 0) == -1 && errno == ESRCH);

    pid_t a = fork();
    if (a == 0) { pause(); _exit(0); }
    pid_t b = fork();
    if (b == 0) { pause(); _exit(0); }

    errno = 0;
    lcheck("joining a group that does not exist is EPERM",
           setpgid(a, 31999) == -1 && errno == EPERM);
    lcheck("a child becomes a group leader", setpgid(a, 0) == 0 && getpgid(a) == a);
    lcheck("a sibling joins that group", setpgid(b, a) == 0 && getpgid(b) == a);
    lcheck("the caller joins it too", setpgid(0, a) == 0 && getpgid(0) == a);

    kill(a, SIGKILL);
    kill(b, SIGKILL);
    waitpid(a, NULL, 0);
    waitpid(b, NULL, 0);
    return local_fails;
}

static int case_default_ignored(void) {
    local_fails = 0;
    raise(SIGWINCH);
    raise(SIGURG);
    nap_ms(20);
    lcheck("still alive after SIGWINCH and SIGURG", 1);
    return local_fails;
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);

    printf("A same session as the parent\n");
    check("getsid(0) == getsid(getppid())", getsid(0) == getsid(getppid()));
    check("getsid(0) == getsid(getpid())", getsid(0) == getsid(getpid()));

    printf("B setsid\n");
    // ash puts each job in a group of its own, so this process leads one.
    if (getpgid(0) == getpid()) {
        errno = 0;
        check("setsid from a group leader is EPERM", setsid() == -1 && errno == EPERM);
    }
    in_child("B setsid (child)", case_setsid);
    in_child("C across sessions", case_cross_session);
    in_child("D groups", case_groups);

    printf("E getsid of nobody\n");
    errno = 0;
    check("getsid(31999) is ESRCH", getsid(31999) == -1 && errno == ESRCH);

    in_child("F default-ignored signals", case_default_ignored);

    printf("G /proc/self/stat\n");
    FILE *f = fopen("/proc/self/stat", "r");
    int pid = 0, ppid = 0, pgrp = 0, session = -1;
    char comm[64], state;
    if (f) {
        fscanf(f, "%d %63s %c %d %d %d", &pid, comm, &state, &ppid, &pgrp, &session);
        fclose(f);
    }
    check("field 6 is the session", session == getsid(0));

    printf("session_test: %s\n", fails ? "FAIL" : "PASS");
    return fails ? 1 : 0;
}
