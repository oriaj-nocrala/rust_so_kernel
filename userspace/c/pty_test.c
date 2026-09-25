// pty_test: pseudo-terminals (phase 3.3 of docs/gui/gui-plan.md) —
// /dev/ptmx, /dev/pts/N, the line discipline of the `tty` crate as the
// kernel runs it, the controlling terminal and job control.
//
//   A. posix_openpt/grantpt/unlockpt/ptsname; the slave is locked until
//      unlockpt; /dev/pts lists it;
//   B. raw mode both ways; O_NONBLOCK master read is EAGAIN;
//   C. canonical mode: echo, \r -> \n, erase, a read never crosses a line;
//   D. ^C typed at the master kills the foreground group with SIGINT;
//   E. a background read stops with SIGTTIN;
//   F. closing the master hangs up the session leader (SIGHUP);
//   G. every slave closed: the master reads EIO and polls POLLHUP;
//   H. TIOCSWINSZ sends SIGWINCH to the foreground group;
//   I. poll on the master is woken by the slave writing;
//   J. /dev/tty is the controlling terminal, ENXIO without one;
//   K. ash on the slave: typed `echo` comes back through the master.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>
#include <dirent.h>
#include <sys/ioctl.h>
#include <sys/wait.h>

static int fails;

static void check(const char *what, int ok) {
    printf("  %s -> %s\n", what, ok ? "PASS" : "FAIL");
    if (!ok) fails++;
}

static long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000L + ts.tv_nsec / 1000000L;
}

static void nap_ms(long ms) {
    struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L };
    nanosleep(&ts, NULL);
}

// A new pair, unlocked; returns the master and fills `name`.
static int new_pair(char *name, size_t len) {
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    if (m < 0) return -1;
    grantpt(m);
    unlockpt(m);
    snprintf(name, len, "%s", ptsname(m));
    return m;
}

// Read from `fd` until `needle` shows up or `ms` pass. Returns bytes read.
static int read_until(int fd, const char *needle, char *buf, int cap, long ms) {
    int got = 0;
    long end = now_ms() + ms;
    buf[0] = 0;
    while (got < cap - 1 && !strstr(buf, needle)) {
        long left = end - now_ms();
        if (left <= 0) break;
        struct pollfd p = { fd, POLLIN, 0 };
        if (poll(&p, 1, (int)left) <= 0) break;
        int n = read(fd, buf + got, cap - 1 - got);
        if (n <= 0) break;
        got += n;
        buf[got] = 0;
    }
    return got;
}

static void make_raw(int fd) {
    struct termios t;
    tcgetattr(fd, &t);
    cfmakeraw(&t);
    tcsetattr(fd, TCSANOW, &t);
}

static void case_open(void) {
    printf("A open\n");
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    check("posix_openpt", m >= 0);
    char *name = ptsname(m);
    check("ptsname is /dev/pts/N", name && strncmp(name, "/dev/pts/", 9) == 0);
    char path[32];
    snprintf(path, sizeof path, "%s", name ? name : "?");
    errno = 0;
    int s = open(path, O_RDWR | O_NOCTTY);
    check("the slave is locked until unlockpt (EIO)", s < 0 && errno == EIO);
    check("unlockpt", unlockpt(m) == 0);
    s = open(path, O_RDWR | O_NOCTTY);
    check("then the slave opens", s >= 0);
    check("isatty on both ends", isatty(m) && isatty(s));

    int listed = 0;
    DIR *d = opendir("/dev/pts");
    if (d) {
        struct dirent *e;
        while ((e = readdir(d)))
            if (strcmp(e->d_name, path + 9) == 0) listed = 1;
        closedir(d);
    }
    check("/dev/pts lists it", listed);
    close(s);
    close(m);
}

static void case_raw(void) {
    printf("B raw mode\n");
    char name[32], buf[64];
    int m = new_pair(name, sizeof name);
    int s = open(name, O_RDWR | O_NOCTTY);
    make_raw(s);
    write(m, "abc", 3);
    int n = read(s, buf, sizeof buf);
    check("master -> slave, untouched", n == 3 && memcmp(buf, "abc", 3) == 0);
    write(s, "x\ny", 3);
    n = read_until(m, "y", buf, sizeof buf, 1000);
    check("slave -> master, no ONLCR in raw", n == 3 && memcmp(buf, "x\ny", 3) == 0);

    fcntl(m, F_SETFL, O_NONBLOCK);
    errno = 0;
    check("O_NONBLOCK master read with nothing is EAGAIN", read(m, buf, 8) == -1 && errno == EAGAIN);
    close(s);
    close(m);
}

static void case_canonical(void) {
    printf("C canonical mode\n");
    char name[32], buf[64];
    int m = new_pair(name, sizeof name);
    int s = open(name, O_RDWR | O_NOCTTY);
    write(m, "hola\r", 5);
    int n = read_until(m, "\n", buf, sizeof buf, 1000);
    check("echo comes back with \\r\\n", n == 6 && memcmp(buf, "hola\r\n", 6) == 0);
    n = read(s, buf, sizeof buf);
    check("the slave reads the line with \\n", n == 5 && memcmp(buf, "hola\n", 5) == 0);

    write(m, "ab\x7f" "c\nsig\n", 9);
    n = read(s, buf, sizeof buf);
    check("erase edited the line", n == 3 && memcmp(buf, "ac\n", 3) == 0);
    n = read(s, buf, sizeof buf);
    check("the next read is the next line", n == 4 && memcmp(buf, "sig\n", 4) == 0);

    write(s, "fin\n", 4);
    read_until(m, "fin\r\n", buf, sizeof buf, 1000);
    check("slave output gets ONLCR", strstr(buf, "fin\r\n") != NULL);
    close(s);
    close(m);
}

// Child side of several cases: a new session whose controlling terminal
// is `name` (acquired by opening it), with stdin/out/err on it.
static int become_session_on(const char *name) {
    setsid();
    int s = open(name, O_RDWR);
    if (s < 0) _exit(90);
    dup2(s, 0);
    dup2(s, 1);
    dup2(s, 2);
    return s;
}

static void case_intr(void) {
    printf("D ^C\n");
    char name[32];
    int m = new_pair(name, sizeof name);
    int ready[2];
    pipe(ready);
    pid_t pid = fork();
    if (pid == 0) {
        close(m);
        become_session_on(name);
        write(ready[1], "r", 1);
        for (;;) pause();
    }
    char c;
    read(ready[0], &c, 1);
    write(m, "\x03", 1);
    int st = 0;
    long t0 = now_ms();
    pid_t w;
    while ((w = waitpid(pid, &st, WNOHANG)) == 0 && now_ms() - t0 < 2000) nap_ms(20);
    check("the foreground group got SIGINT", w == pid && WIFSIGNALED(st) && WTERMSIG(st) == SIGINT);
    if (w != pid) { kill(pid, SIGKILL); waitpid(pid, NULL, 0); }
    close(m);
}

static void case_ttin(void) {
    printf("E background read\n");
    char name[32];
    int m = new_pair(name, sizeof name);
    pid_t pid = fork();
    if (pid == 0) {
        close(m);
        become_session_on(name);
        pid_t gc = fork();
        if (gc == 0) {
            setpgid(0, 0); // a background group of the same session
            char b[8];
            read(0, b, sizeof b);
            _exit(0);
        }
        int st = 0;
        pid_t w = waitpid(gc, &st, WUNTRACED);
        int ok = w == gc && WIFSTOPPED(st) && WSTOPSIG(st) == SIGTTIN;
        kill(gc, SIGKILL);
        waitpid(gc, NULL, 0);
        _exit(ok ? 0 : 1);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    check("stopped with SIGTTIN", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    close(m);
}

static void case_hangup(void) {
    printf("F master closed\n");
    char name[32];
    int m = new_pair(name, sizeof name);
    int ready[2];
    pipe(ready);
    pid_t pid = fork();
    if (pid == 0) {
        close(m);
        become_session_on(name);
        write(ready[1], "r", 1);
        for (;;) pause();
    }
    char c;
    read(ready[0], &c, 1);
    close(m);
    int st = 0;
    long t0 = now_ms();
    pid_t w;
    while ((w = waitpid(pid, &st, WNOHANG)) == 0 && now_ms() - t0 < 2000) nap_ms(20);
    check("the session leader got SIGHUP", w == pid && WIFSIGNALED(st) && WTERMSIG(st) == SIGHUP);
    if (w != pid) { kill(pid, SIGKILL); waitpid(pid, NULL, 0); }
}

static void case_slave_gone(void) {
    printf("G slave closed\n");
    char name[32], buf[16];
    int m = new_pair(name, sizeof name);
    int s = open(name, O_RDWR | O_NOCTTY);
    write(s, "z", 1);
    close(s);
    struct pollfd p = { m, POLLIN, 0 };
    int r = poll(&p, 1, 1000);
    check("poll reports POLLHUP", r == 1 && (p.revents & POLLHUP));
    int n = read(m, buf, sizeof buf);
    check("what was written first is still read", n == 1 && buf[0] == 'z');
    errno = 0;
    check("then the master reads EIO", read(m, buf, sizeof buf) == -1 && errno == EIO);
    close(m);
}

static volatile sig_atomic_t winch;
static void on_winch(int sig) { (void)sig; winch++; }

static void case_winch(void) {
    printf("H SIGWINCH\n");
    char name[32];
    int m = new_pair(name, sizeof name);
    int ready[2];
    pipe(ready);
    pid_t pid = fork();
    if (pid == 0) {
        close(m);
        signal(SIGWINCH, on_winch);
        become_session_on(name);
        write(ready[1], "r", 1);
        long t0 = now_ms();
        while (!winch && now_ms() - t0 < 2000) nap_ms(10);
        struct winsize ws;
        ioctl(0, TIOCGWINSZ, &ws);
        _exit(winch && ws.ws_row == 25 && ws.ws_col == 80 ? 0 : 1);
    }
    char c;
    read(ready[0], &c, 1);
    struct winsize ws = { 25, 80, 0, 0 };
    ioctl(m, TIOCSWINSZ, &ws);
    int st = 0;
    waitpid(pid, &st, 0);
    check("the foreground group got SIGWINCH and the new size", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    close(m);
}

static void case_poll(void) {
    printf("I poll woken by the other end\n");
    char name[32], buf[8];
    int m = new_pair(name, sizeof name);
    int s = open(name, O_RDWR | O_NOCTTY);
    pid_t pid = fork();
    if (pid == 0) {
        nap_ms(200);
        write(s, "w", 1);
        _exit(0);
    }
    struct pollfd p = { m, POLLIN, 0 };
    long t0 = now_ms();
    int r = poll(&p, 1, 3000);
    long waited = now_ms() - t0;
    check("poll returns POLLIN when the slave writes", r == 1 && (p.revents & POLLIN));
    check("woken, not timed out", waited >= 150 && waited < 2000);
    read(m, buf, sizeof buf);
    waitpid(pid, NULL, 0);
    close(s);
    close(m);
}

static void case_dev_tty(void) {
    printf("J /dev/tty\n");
    errno = 0;
    int t = open("/dev/tty", O_RDWR);
    if (t < 0) {
        check("no controlling terminal: ENXIO", errno == ENXIO);
    } else {
        // Started from a pty: fine, it just has one.
        close(t);
        check("open /dev/tty (this process has a controlling terminal)", 1);
    }
    char name[32];
    int m = new_pair(name, sizeof name);
    pid_t pid = fork();
    if (pid == 0) {
        close(m);
        become_session_on(name);
        int t2 = open("/dev/tty", O_RDWR);
        pid_t fg = tcgetpgrp(0);
        _exit(t2 >= 0 && fg == getpgrp() && tcgetsid(0) == getpid() ? 0 : 1);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    check("in the new session /dev/tty opens, and it is the foreground", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    close(m);
}

static void case_ash(void) {
    printf("K ash on the slave\n");
    char name[32], buf[4096];
    int m = new_pair(name, sizeof name);
    pid_t pid = fork();
    if (pid == 0) {
        close(m);
        become_session_on(name);
        char *argv[] = { "ash", "-i", NULL };
        char *envp[] = { "PATH=/tmp/bin:/bin:/mnt/bin", "PS1=pty$ ", NULL };
        execve("/bin/busybox", argv, envp);
        _exit(91);
    }
    read_until(m, "pty$ ", buf, sizeof buf, 3000);
    check("ash printed its prompt", strstr(buf, "pty$ ") != NULL);
    const char *cmd = "echo hola$((40+2))\r";
    write(m, cmd, strlen(cmd));
    read_until(m, "hola42\r\n", buf, sizeof buf, 3000);
    check("the command's output came back", strstr(buf, "hola42\r\n") != NULL);
    write(m, "exit\r", 5);
    int st = 0;
    long t0 = now_ms();
    pid_t w;
    while ((w = waitpid(pid, &st, WNOHANG)) == 0 && now_ms() - t0 < 3000) {
        read_until(m, "\x01", buf, sizeof buf, 50); // keep draining
    }
    check("ash exited", w == pid && WIFEXITED(st));
    if (w != pid) { kill(pid, SIGKILL); waitpid(pid, NULL, 0); }
    close(m);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    case_open();
    case_raw();
    case_canonical();
    case_intr();
    case_ttin();
    case_hangup();
    case_slave_gone();
    case_winch();
    case_poll();
    case_dev_tty();
    case_ash();
    printf("pty_test: %s\n", fails ? "FAIL" : "PASS");
    return fails ? 1 : 0;
}
