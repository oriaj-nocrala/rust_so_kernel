// FD_CLOEXEC: open(O_CLOEXEC), fcntl(F_GETFD/F_SETFD/F_DUPFD_CLOEXEC), dup/dup2/dup3, pipe2, memfd_create(MFD_CLOEXEC),
// socket(SOCK_CLOEXEC), and what exec does with them. The fd table has 16 slots, so the layout below is tight.
// The parent execs this same program with "child"; the child reports which fds it still has open through fd 15.
#include <stdio.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/wait.h>

static long sc(long nr, long a, long b, long c) {
    long ret;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(nr), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return ret;
}
enum { SYS_dup3 = 292, SYS_pipe2 = 293, SYS_memfd_create = 319, SYS_socket = 41 };
enum { O_CLOEXEC_ = 0x80000, SOCK_CLOEXEC_ = 0x80000, MFD_CLOEXEC_ = 1, EINVAL_ = 22, EBADF_ = 9 };

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

static int getfd(int fd) { return fcntl(fd, F_GETFD); }

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "child")) {
        unsigned short open_mask = 0;
        for (int fd = 3; fd < 16; fd++)
            if (getfd(fd) >= 0) open_mask |= 1u << fd;
        write(15, &open_mask, sizeof open_mask);
        return 0;
    }

    printf("cloexec\n");
    int a = open("/dev/null", O_RDONLY | O_CLOEXEC);          // 3
    int b = open("/dev/null", O_RDONLY);                      // 4
    CHECK(a == 3 && b == 4, "fds %d %d", a, b);
    CHECK(getfd(a) == FD_CLOEXEC && getfd(b) == 0, "open flags: %d %d", getfd(a), getfd(b));
    int d = dup(a);                                           // 5: a copy does not inherit the flag
    CHECK(d == 5 && getfd(d) == 0, "dup: fd %d flag %d", d, getfd(d));
    int pp[2];
    CHECK(sc(SYS_pipe2, (long)pp, O_CLOEXEC_, 0) == 0 && pp[0] == 6 && pp[1] == 7, "pipe2 gave %d %d", pp[0], pp[1]);
    CHECK(getfd(6) == 1 && getfd(7) == 1, "pipe2 O_CLOEXEC on both ends");
    int g = (int)sc(SYS_memfd_create, (long)"x", MFD_CLOEXEC_, 0);   // 8
    CHECK(g == 8 && getfd(g) == 1, "memfd_create fd %d flag %d", g, getfd(g));
    int h = (int)sc(SYS_socket, 1, 1 | SOCK_CLOEXEC_, 0);            // 9
    CHECK(h == 9 && getfd(h) == 1, "socket fd %d flag %d", h, getfd(h));
    int c = fcntl(b, F_DUPFD_CLOEXEC, 10);                    // 10
    CHECK(c == 10 && getfd(c) == 1, "F_DUPFD_CLOEXEC fd %d flag %d", c, getfd(c));
    int e = (int)sc(SYS_dup3, b, 12, O_CLOEXEC_);
    CHECK(e == 12 && getfd(e) == 1, "dup3 fd %d flag %d", e, getfd(e));
    CHECK(sc(SYS_dup3, b, b, 0) == -EINVAL_, "dup3 with oldfd == newfd");
    CHECK(sc(SYS_dup3, b, 14, 1) == -EINVAL_, "dup3 with an unknown flag");
    int f = dup2(a, 13);                                      // dup2 never sets it
    CHECK(f == 13 && getfd(f) == 0, "dup2 fd %d flag %d", f, getfd(f));
    CHECK(sc(SYS_pipe2, (long)pp, 0x4000, 0) == -EINVAL_, "pipe2 with an unknown flag");
    CHECK(sc(SYS_dup3, b, 11, 0) == 11 && getfd(11) == 0, "dup3 without the flag (also fills slot 11)");

    CHECK(fcntl(a, F_SETFD, 0) == 0 && getfd(a) == 0, "F_SETFD clears the flag");
    CHECK(fcntl(b, F_SETFD, FD_CLOEXEC) == 0 && getfd(b) == 1, "F_SETFD sets the flag");
    CHECK(getfd(99) == -1, "F_GETFD of a closed fd");

    int rp[2];
    CHECK(pipe(rp) == 0 && rp[0] == 14 && rp[1] == 15, "report pipe %d %d", rp[0], rp[1]);

    // a failed exec must leave everything open
    pid_t pid = fork();
    if (pid == 0) {
        execl("/mnt/bin/no_such_program", "x", (char *)0);
        _exit(getfd(b) == 1 && getfd(g) == 1 && getfd(h) == 1 ? 0 : 1);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "a failed exec closed a close-on-exec fd (status %#x)", st);

    pid = fork();
    if (pid == 0) {
        execl("/mnt/bin/cloexec_test", "cloexec_test", "child", (char *)0);
        _exit(99);
    }
    close(rp[1]);
    unsigned short mask = 0;
    CHECK(read(rp[0], &mask, sizeof mask) == 2, "the exec'd child reported");
    waitpid(pid, &st, 0);
    unsigned short want = (1u << 3) | (1u << 5) | (1u << 11) | (1u << 13) | (1u << 14) | (1u << 15);   // a, d, the plain dup3, f and the report pipe
    CHECK(mask == want, "open after exec: %#x, wanted %#x", mask, want);

    if (failures) {
        printf("cloexec_test: %d FAILED\n", failures);
        return 1;
    }
    printf("cloexec_test: OK\n");
    return 0;
}
