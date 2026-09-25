// lifecycle_test: what happens to a process's children and handlers when
// it exits or execs.
//
//   A. a child that forks a grandchild and exits without reaping it: the
//      grandchild's zombie is handed to PID 1 and reaped (its /proc entry
//      goes away) instead of sitting in the wait queue forever;
//   B. a grandchild still running when its parent exits is adopted by
//      PID 1 (ppid in /proc/<pid>/stat becomes 1);
//   C. exec resets a caught signal to SIG_DFL: the exec'd image raises
//      SIGUSR1 and must die of it, not jump to the old image's handler;
//   D. exec keeps an ignored signal ignored: SIGUSR2 raised after exec
//      does nothing.
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <sys/socket.h>

static void nap_ms(long ms) {
    struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L };
    nanosleep(&ts, NULL);
}

static int proc_exists(int pid) {
    char path[32];
    struct stat st;
    snprintf(path, sizeof path, "/proc/%d/stat", pid);
    return stat(path, &st) == 0;
}

// Field 4 of /proc/<pid>/stat; -1 if unreadable.
static int proc_ppid(int pid) {
    char path[32], buf[256];
    snprintf(path, sizeof path, "/proc/%d/stat", pid);
    FILE *f = fopen(path, "r");
    if (!f) return -1;
    size_t n = fread(buf, 1, sizeof buf - 1, f);
    fclose(f);
    buf[n] = 0;
    char *p = strrchr(buf, ')');
    int ppid = -1;
    char state;
    if (!p || sscanf(p + 1, " %c %d", &state, &ppid) != 2) return -1;
    return ppid;
}

// The middle process: forks a grandchild, reports its pid through the
// pipe, and exits without waiting for it.
static int spawn_orphan(long grandchild_ms) {
    int p[2];
    pipe(p);
    pid_t mid = fork();
    if (mid == 0) {
        close(p[0]);
        pid_t g = fork();
        if (g == 0) {
            if (grandchild_ms) nap_ms(grandchild_ms);
            _exit(0);
        }
        write(p[1], &g, sizeof g);
        if (!grandchild_ms) nap_ms(200); // let the grandchild die first: a zombie when orphaned
        _exit(0);
    }
    close(p[1]);
    pid_t g = -1;
    read(p[0], &g, sizeof g);
    close(p[0]);
    waitpid(mid, NULL, 0);
    return g;
}

static int case_zombie_orphan(void) {
    int g = spawn_orphan(0);
    nap_ms(300);
    int gone = !proc_exists(g);
    printf("A zombie orphan: grandchild %d reaped=%d -> %s\n", g, gone, gone ? "PASS" : "FAIL");
    return !gone;
}

static int case_live_orphan(void) {
    int g = spawn_orphan(600);
    int ppid = proc_ppid(g);
    nap_ms(900);
    int gone = !proc_exists(g);
    int ok = ppid == 1 && gone;
    printf("B live orphan: grandchild %d ppid=%d reaped_after_exit=%d -> %s\n",
           g, ppid, gone, ok ? "PASS" : "FAIL");
    return !ok;
}

static void on_usr1(int sig) { (void)sig; }

static int case_exec(const char *self, const char *mode, int want_signal) {
    pid_t pid = fork();
    if (pid == 0) {
        struct sigaction sa;
        memset(&sa, 0, sizeof sa);
        sa.sa_handler = want_signal ? on_usr1 : SIG_IGN;
        sigaction(want_signal ? SIGUSR1 : SIGUSR2, &sa, NULL);
        execl(self, self, mode, (char *)NULL);
        _exit(126);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    int ok = want_signal ? (WIFSIGNALED(status) && WTERMSIG(status) == SIGUSR1)
                         : (WIFEXITED(status) && WEXITSTATUS(status) == 0);
    printf("%s: status=%#x -> %s\n",
           want_signal ? "C exec resets caught handler" : "D exec keeps SIG_IGN",
           status, ok ? "PASS" : "FAIL");
    return !ok;
}

// E: a child killed by SIGKILL while holding a socket and a pipe. Its files
// must close at death, as on Linux -- the parent sees EOF on both *before*
// reaping it -- and reaping it must not hang. Until 2026-09-25 a signal
// death kept the fd table in the zombie and the reap dropped it under the
// scheduler lock: a socket's Drop takes that lock again, and every CPU hung.
static int case_killed_with_files(void) {
    int sv[2], pp[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) < 0 || pipe(pp) < 0) {
        printf("E killed child's files close: setup failed -> FAIL\n");
        return 1;
    }
    int pid = fork();
    if (pid == 0) {
        close(sv[0]);
        close(pp[0]);
        char c;
        read(sv[1], &c, 1); // sleeps holding sv[1] and pp[1] until killed
        _exit(0);
    }
    close(sv[1]);
    close(pp[1]);
    nap_ms(100);
    kill(pid, SIGKILL);
    // EOF with the zombie still unreaped. Never block here: without the fix
    // EOF only comes at the reap, and a blocking read would hang the test
    // instead of failing it. The socket is polled with MSG_DONTWAIT; the
    // pipe (whose poll is always "ready" here) is read only once the socket
    // saw EOF -- both close together, when the dead table is dropped.
    int eof_sock = 0, eof_pipe = 0;
    char c;
    for (int i = 0; i < 40 && !eof_sock; i++) {
        if (recv(sv[0], &c, 1, MSG_DONTWAIT) == 0) eof_sock = 1;
        else nap_ms(50);
    }
    if (eof_sock && read(pp[0], &c, 1) == 0) eof_pipe = 1;
    int status = 0;
    int r = waitpid(pid, &status, 0);
    int ok = eof_sock && eof_pipe && r == pid && WIFSIGNALED(status) && WTERMSIG(status) == SIGKILL;
    printf("E killed child's files close: socket EOF %d, pipe EOF %d, reaped %d -> %s\n",
           eof_sock, eof_pipe, r == pid, ok ? "PASS" : "FAIL");
    close(sv[0]);
    close(pp[0]);
    return !ok;
}

int main(int argc, char **argv) {
    if (argc > 1) {
        // Exec'd image for C/D: raise and see what the kernel does with it.
        raise(strcmp(argv[1], "usr1") == 0 ? SIGUSR1 : SIGUSR2);
        return 0;
    }
    const char *self = "/mnt/bin/lifecycle_test";
    int fails = 0;
    fails += case_zombie_orphan();
    fails += case_live_orphan();
    fails += case_exec(self, "usr1", 1);
    fails += case_exec(self, "usr2", 0);
    fails += case_killed_with_files();
    printf("lifecycle_test: %s\n", fails ? "FAIL" : "PASS");
    return fails ? 1 : 0;
}
