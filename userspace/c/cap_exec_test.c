// execveat(322) and cap-exec. execveat: AT_EMPTY_PATH runs the file behind a descriptor (fexecve), needs CAP_FEXECVE,
// leaves the descriptor's offset alone, works in capability mode where execve is refused. cap-exec: the program it starts
// is in capability mode, has exactly the directories and fds it was given (with their rights, named in CAPEXEC_FDS),
// every other inherited fd closed, stdio narrowed; it cannot reach anything else (the handoff's test: `cat` of a file
// outside fails, a file under the given directory is readable through the dirfd); a bad argument says what was wrong.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <stdlib.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/wait.h>
#include <sys/stat.h>
#include "constanos_capsicum.h"

static long sc(long nr, long a, long b, long c) {
    long ret;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(nr), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return ret;
}
static long sc5(long nr, long a, long b, long c, long d, long e) {
    long ret;
    register long r10 __asm__("r10") = d;
    register long r8 __asm__("r8") = e;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8) : "rcx", "r11", "memory");
    return ret;
}

enum { SYS_read = 0, SYS_write = 1, SYS_close = 3, SYS_lseek = 8, SYS_openat = 257, SYS_execveat = 322,
       SYS_cap_enter = 407, SYS_cap_getmode = 408, AT_FDCWD_ = -100, AT_EMPTY_PATH_ = 0x1000 };

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

static const char *slurp(long fd) {
    static char buf[64];
    if (fd < 0) return "";
    long n = sc(SYS_read, fd, (long)buf, sizeof buf - 1);
    sc(SYS_close, fd, 0, 0);
    buf[n > 0 ? n : 0] = 0;
    return buf;
}

static cap_rights_t rights_of(long fd) {
    cap_rights_t r = 0;
    return cap_rights_get((int)fd, &r) == 0 ? r : 0xbad;
}

#define RO (CAP_LOOKUP | CAP_READ | CAP_SEEK | CAP_FSTAT | CAP_EVENT | CAP_FCNTL | CAP_MMAP | CAP_FCHDIR)

// Run argv (argv[0] a path) and return its exit code, or 128 + signal.
static int run(char *const argv[]) {
    pid_t pid = fork();
    if (pid == 0) {
        execv(argv[0], argv);
        _exit(126);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    return WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
}

// Started by cap-exec: check what we were handed. Exit 42 when everything holds, else the number of the first failed
// check (so the parent can say which).
static int sandboxed(void) {
    unsigned mode = 0;
    sc(SYS_cap_getmode, (long)&mode, 0, 0);
    if (mode != 1) return 1;
    const char *fds = getenv("CAPEXEC_FDS");
    if (!fds || strcmp(fds, "3=/tmp/cx 7=fd")) return 2;
    if (strcmp(slurp(sc(SYS_openat, 3, (long)"f", O_RDONLY)), "inside")) return 3;
    if (sc(SYS_openat, AT_FDCWD_, (long)"/tmp/cxout", O_RDONLY) != -ECAPMODE) return 4;
    if (sc(SYS_openat, 3, (long)"../cxout", O_RDONLY) != -ENOTCAPABLE) return 5;
    if (rights_of(3) != RO) return 6;
    if (sc(SYS_openat, 3, (long)"new", O_WRONLY | O_CREAT) != -ENOTCAPABLE) return 7; // ro: no CAP_CREATE
    // fd 7 was granted explicitly with read only: readable, not writable.
    char c;
    if (rights_of(7) != CAP_READ || sc(SYS_read, 7, (long)&c, 1) != 1 || c != 'o') return 8;
    if (sc(SYS_write, 7, (long)"x", 1) != -ENOTCAPABLE) return 9;
    // fd 5, open in the parent but not granted, is gone.
    if (rights_of(5) != 0xbad) return 10;
    // stdio narrowed: stdout writes, stdin cannot be written.
    if (sc(SYS_write, 1, (long)"", 0) != 0) return 11;
    if (sc(SYS_write, 0, (long)"x", 1) != -ENOTCAPABLE) return 12;
    return 42;
}

int main(int argc, char **argv) {
    if (argc == 2 && !strcmp(argv[1], "child")) return sandboxed();
    if (argc == 2 && !strcmp(argv[1], "exit7")) return 7;

    rmdir("/tmp/cx/new");
    mkdir("/tmp/cx", 0755);
    int w = open("/tmp/cx/f", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    write(w, "inside", 6); close(w);
    w = open("/tmp/cxout", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    write(w, "outside", 7); close(w);

    printf("execveat\n");
    {
        long self = sc(SYS_openat, AT_FDCWD_, (long)"/mnt/bin/cap_exec_test", O_RDONLY);
        CHECK(self >= 0, "open our own binary: %ld", self);
        CHECK(sc(SYS_lseek, self, 100, SEEK_SET) == 100, "move its offset to 100");
        char *args[] = { "cap_exec_test", "exit7", NULL };
        pid_t pid = fork();
        if (pid == 0) _exit((int)-sc5(SYS_execveat, self, (long)"", (long)args, (long)environ, AT_EMPTY_PATH_) + 100);
        int st = 0;
        waitpid(pid, &st, 0);
        CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 7, "execveat(fd, \"\", AT_EMPTY_PATH) runs it (status %#x)", st);
        CHECK(sc(SYS_lseek, self, 0, SEEK_CUR) == 100, "and the parent's offset is still 100");
        CHECK(sc5(SYS_execveat, self, (long)"", (long)args, (long)environ, 0) == -ENOENT, "an empty path without AT_EMPTY_PATH: ENOENT");
        CHECK(sc5(SYS_execveat, self, (long)"", (long)args, (long)environ, 0x40000) == -EINVAL, "an unknown flag: EINVAL");

        pid = fork();
        if (pid == 0) {
            // In capability mode: execve is refused, execveat from the fd is not.
            sc(SYS_cap_enter, 0, 0, 0);
            if (sc(59, (long)"/mnt/bin/cap_exec_test", (long)args, 0) != -ECAPMODE) _exit(50);
            sc5(SYS_execveat, self, (long)"", (long)args, (long)environ, AT_EMPTY_PATH_);
            _exit(51);
        }
        waitpid(pid, &st, 0);
        CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 7, "in capability mode, execve is refused and execveat from the fd runs (status %#x)", st);

        cap_rights_limit((int)self, CAP_READ | CAP_SEEK);
        CHECK(sc5(SYS_execveat, self, (long)"", (long)args, (long)environ, AT_EMPTY_PATH_) == -ENOTCAPABLE, "without CAP_FEXECVE: ENOTCAPABLE");
        sc(SYS_close, self, 0, 0);
        long dir = sc(SYS_openat, AT_FDCWD_, (long)"/tmp/cx", O_RDONLY | O_DIRECTORY);
        CHECK(sc5(SYS_execveat, dir, (long)"", (long)args, (long)environ, AT_EMPTY_PATH_) == -EACCES, "a directory: EACCES");
        sc(SYS_close, dir, 0, 0);
    }

    printf("cap-exec\n");
    {
        // fd 5: open here, not granted (must be closed in the sandbox). fd 7: granted read-only.
        int five = open("/tmp/cxout", O_RDONLY);
        dup2(five, 5);
        if (five != 5) close(five);
        int seven = open("/tmp/cxout", O_RDONLY);
        dup2(seven, 7);
        if (seven != 7) close(seven);
        char *args[] = { "/mnt/bin/cap-exec", "--fd", "7:read", "--dir", "/tmp/cx", "--", "/mnt/bin/cap_exec_test", "child", NULL };
        int code = run(args);
        CHECK(code == 42, "the sandboxed program sees exactly what it was given (exit %d: 42 is all good, else the failed check)", code);
        close(5); close(7);

        // The handoff's test: cat of a file outside the directory fails; inside, through the dirfd, works (sandboxed()).
        char *cat[] = { "/mnt/bin/cap-exec", "--dir", "/tmp/cx", "--", "busybox", "cat", "/tmp/cxout", NULL };
        code = run(cat);
        CHECK(code != 0, "cap-exec ... -- busybox cat /tmp/cxout fails (exit %d)", code);

        char *bad[] = { "/mnt/bin/cap-exec", "--dir", "/nonexistent", "--", "/mnt/bin/hello", NULL };
        CHECK(run(bad) == 127, "a directory that cannot be opened: exit 127 (and a message saying which)");
        char *badright[] = { "/mnt/bin/cap-exec", "--dir", "/tmp/cx:flying", "--", "/mnt/bin/hello", NULL };
        CHECK(run(badright) == 127, "an unknown right: exit 127");
        char *noprog[] = { "/mnt/bin/cap-exec", "--", "no-such-program-xyz", NULL };
        CHECK(run(noprog) == 127, "a program not in PATH: exit 127");
    }

    printf(failures ? "cap_exec_test: FAIL\n" : "cap_exec_test: PASS\n");
    return failures != 0;
}
