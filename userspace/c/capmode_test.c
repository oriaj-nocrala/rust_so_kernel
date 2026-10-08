// Capability mode (cap_enter 407 / cap_getmode 408): a child enters it and tries every escape: paths from / and from the
// cwd, `..` and symlinks out of its dirfd, /dev and /proc, another pid, connect/bind/sendto by address, chdir, getcwd,
// exec, setuid, pidfd_open. Each fails (ECAPMODE, or ENOTCAPABLE for leaving the dirfd), while work through the dirfd
// still succeeds. Forked children inherit the mode, the parent does not get it, a sibling thread's cap_enter covers the
// whole process. Then the parent checks that each refusal left its record in /proc/capdenials. Raw syscalls.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <stdlib.h>
#include <unistd.h>
#include <fcntl.h>
#include <pthread.h>
#include <sys/wait.h>
#include <sys/stat.h>
#include <sys/socket.h>
#include "constanos_capsicum.h"

static long sc(long nr, long a, long b, long c) {
    long ret;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(nr), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return ret;
}
static long sc6(long nr, long a, long b, long c, long d, long e, long f) {
    long ret;
    register long r10 __asm__("r10") = d;
    register long r8 __asm__("r8") = e;
    register long r9 __asm__("r9") = f;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8), "r"(r9) : "rcx", "r11", "memory");
    return ret;
}

enum {
    SYS_read = 0, SYS_open = 2, SYS_close = 3, SYS_stat = 4, SYS_socket = 41, SYS_connect = 42, SYS_sendto = 44,
    SYS_bind = 49, SYS_execve = 59, SYS_kill = 62, SYS_getcwd = 79, SYS_chdir = 80, SYS_setuid = 105,
    SYS_openat = 257, SYS_mkdirat = 258, SYS_newfstatat = 262, SYS_unlinkat = 263, SYS_pidfd_open = 434,
    SYS_cap_enter = 407, SYS_cap_getmode = 408, SYS_openat2 = 437,
    AT_FDCWD_ = -100, AT_SYMLINK_NOFOLLOW_ = 0x100, AT_REMOVEDIR_ = 0x200,
};

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

static int getmode(void) {
    unsigned m = 99;
    sc(SYS_cap_getmode, (long)&m, 0, 0);
    return (int)m;
}

static const char *slurp(long fd) {
    static char buf[32];
    if (fd < 0) return "";
    long n = sc(SYS_read, fd, (long)buf, sizeof buf - 1);
    sc(SYS_close, fd, 0, 0);
    buf[n > 0 ? n : 0] = 0;
    return buf;
}

static void put(const char *path, const char *text) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    write(fd, text, strlen(text));
    close(fd);
}

struct sockaddr_un_ { unsigned short fam; char path[108]; };

static void *thread_enters(void *arg) {
    (void)arg;
    sc(SYS_cap_enter, 0, 0, 0);
    return NULL;
}

// Everything the child in capability mode does. Returns its failure count.
static int in_capmode(long d, pid_t parent) {
    CHECK(sc(SYS_cap_enter, 0, 0, 0) == 0, "cap_enter");
    CHECK(getmode() == 1, "cap_getmode says 1");

    printf("through the dirfd\n");
    CHECK(!strcmp(slurp(sc(SYS_openat, d, (long)"f", O_RDONLY)), "inside"), "openat(d, f) reads 'inside'");
    CHECK(!strcmp(slurp(sc(SYS_openat, d, (long)"sub/../f", O_RDONLY)), "inside"), "sub/../f stays inside");
    long r = sc(SYS_mkdirat, d, (long)"new", 0755);
    CHECK(r == 0, "mkdirat(d, new): %ld", r);
    CHECK(sc(SYS_unlinkat, d, (long)"new", AT_REMOVEDIR_) == 0, "unlinkat(d, new, AT_REMOVEDIR)");
    struct stat st;
    CHECK(sc6(SYS_newfstatat, d, (long)"abs", (long)&st, AT_SYMLINK_NOFOLLOW_, 0, 0) == 0, "fstatat NOFOLLOW of an outward symlink: the link itself is inside");

    printf("paths from / or the cwd\n");
    CHECK(sc(SYS_open, (long)"/tmp/cmout", O_RDONLY, 0) == -ECAPMODE, "open(/tmp/cmout): ECAPMODE");
    CHECK(sc(SYS_openat, AT_FDCWD_, (long)"f", O_RDONLY) == -ECAPMODE, "openat(AT_FDCWD, f): ECAPMODE");
    CHECK(sc(SYS_openat, d, (long)"/tmp/cmout", O_RDONLY) == -ECAPMODE, "openat(d, absolute): ECAPMODE");
    CHECK(sc(SYS_openat, AT_FDCWD_, (long)"/dev/null", O_RDONLY) == -ECAPMODE, "/dev/null: ECAPMODE");
    CHECK(sc(SYS_openat, AT_FDCWD_, (long)"/proc/self/maps", O_RDONLY) == -ECAPMODE, "/proc/self: ECAPMODE");
    CHECK(sc(SYS_stat, (long)"/etc", (long)&st, 0) == -ECAPMODE, "stat(/etc): ECAPMODE");
    {
        struct { uint64_t flags, mode, resolve; } how = { O_RDONLY, 0, 0 };
        CHECK(sc6(SYS_openat2, AT_FDCWD_, (long)"/tmp/cmout", (long)&how, sizeof how, 0, 0) == -ECAPMODE, "openat2 absolute: ECAPMODE");
        CHECK(sc6(SYS_openat2, d, (long)"../cmout", (long)&how, sizeof how, 0, 0) == -ENOTCAPABLE, "openat2(d, ../cmout), no RESOLVE_ bits: ENOTCAPABLE, as openat");
        how.resolve = 8; // RESOLVE_BENEATH asked for: its own error
        CHECK(sc6(SYS_openat2, d, (long)"../cmout", (long)&how, sizeof how, 0, 0) == -EXDEV, "openat2(d, ../cmout, RESOLVE_BENEATH): EXDEV");
        how.resolve = 4; // RESOLVE_NO_SYMLINKS only: capability mode adds BENEATH
        CHECK(sc6(SYS_openat2, d, (long)"../cmout", (long)&how, sizeof how, 0, 0) == -EXDEV, "openat2(d, ../cmout, RESOLVE_NO_SYMLINKS): beneath anyway (EXDEV)");
    }

    printf("leaving the dirfd\n");
    r = sc(SYS_openat, d, (long)"..", O_RDONLY);
    CHECK(r == -ENOTCAPABLE, "openat(d, ..): ENOTCAPABLE (%ld)", r);
    CHECK(sc(SYS_openat, d, (long)"../cmout", O_RDONLY) == -ENOTCAPABLE, "openat(d, ../cmout): ENOTCAPABLE");
    CHECK(sc(SYS_openat, d, (long)"up", O_RDONLY) == -ENOTCAPABLE, "a relative symlink out: ENOTCAPABLE");
    CHECK(sc(SYS_openat, d, (long)"abs", O_RDONLY) == -ENOTCAPABLE, "an absolute symlink: ENOTCAPABLE");
    CHECK(sc6(SYS_newfstatat, d, (long)"abs", (long)&st, 0, 0, 0) == -ENOTCAPABLE, "fstatat following the absolute symlink: ENOTCAPABLE");
    CHECK(sc(SYS_mkdirat, d, (long)"../cmnew", 0755) == -ENOTCAPABLE, "mkdirat(d, ../cmnew): ENOTCAPABLE");

    printf("processes, addresses, the rest\n");
    CHECK(sc(SYS_kill, parent, 0, 0) == -ECAPMODE, "kill(parent, 0): ECAPMODE");
    CHECK(sc(SYS_kill, getpid(), 0, 0) == 0, "kill(self, 0) works");
    CHECK(sc(SYS_pidfd_open, parent, 0, 0) == -ECAPMODE, "pidfd_open(parent): ECAPMODE");
    long s = sc(SYS_socket, AF_UNIX, SOCK_STREAM, 0);
    CHECK(s >= 0, "socket() itself works");
    struct sockaddr_un_ addr = { AF_UNIX, "/tmp/cm/sock" };
    CHECK(sc(SYS_connect, s, (long)&addr, sizeof addr) == -ECAPMODE, "connect by path: ECAPMODE");
    CHECK(sc(SYS_bind, s, (long)&addr, sizeof addr) == -ECAPMODE, "bind: ECAPMODE");
    long ds = sc(SYS_socket, AF_UNIX, SOCK_DGRAM, 0);
    CHECK(sc6(SYS_sendto, ds, (long)"x", 1, 0, (long)&addr, sizeof addr) == -ECAPMODE, "sendto an address: ECAPMODE");
    CHECK(sc(SYS_chdir, (long)"/", 0, 0) == -ECAPMODE, "chdir: ECAPMODE");
    char cwd[64];
    CHECK(sc(SYS_getcwd, (long)cwd, sizeof cwd, 0) == -ECAPMODE, "getcwd: ECAPMODE");
    char *argv[] = { "/mnt/bin/hello", NULL };
    CHECK(sc(SYS_execve, (long)"/mnt/bin/hello", (long)argv, 0) == -ECAPMODE, "execve: ECAPMODE");
    CHECK(sc(SYS_setuid, 0, 0, 0) == -ECAPMODE, "setuid: ECAPMODE");

    printf("fork inherits\n");
    pid_t g = fork();
    if (g == 0) _exit(getmode() == 1 && sc(SYS_open, (long)"/tmp/cmout", O_RDONLY, 0) == -ECAPMODE ? 42 : 43);
    int status = 0;
    waitpid(g, &status, 0);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 42, "a grandchild is in capability mode too (status %#x)", status);
    return failures;
}

static int count_lines_with(const char *text, pid_t pid, const char *needle) {
    char prefix[16];
    snprintf(prefix, sizeof prefix, "%d ", pid);
    int n = 0;
    for (const char *line = text; *line; ) {
        const char *nl = strchr(line, '\n');
        size_t len = nl ? (size_t)(nl - line) : strlen(line);
        if (!strncmp(line, prefix, strlen(prefix)) && memmem(line, len, needle, strlen(needle))) n++;
        if (!nl) break;
        line = nl + 1;
    }
    return n;
}

int main(void) {
    unlink("/tmp/cm/abs"); unlink("/tmp/cm/up"); unlink("/tmp/cm/sock"); rmdir("/tmp/cm/new"); rmdir("/tmp/cmnew");
    mkdir("/tmp/cm", 0755); mkdir("/tmp/cm/sub", 0755);
    put("/tmp/cm/f", "inside");
    put("/tmp/cmout", "outside");
    symlink("/tmp/cmout", "/tmp/cm/abs");
    symlink("../cmout", "/tmp/cm/up");
    long d = sc(SYS_openat, AT_FDCWD_, (long)"/tmp/cm", O_RDONLY | O_DIRECTORY);
    CHECK(d >= 0, "open /tmp/cm: %ld", d);
    CHECK(getmode() == 0, "not in capability mode to begin with");

    // The child says it reached the end through a pipe: an exit status alone would also be 0 if a wrongly allowed execve
    // had replaced it with a program that exits 0.
    int done[2];
    pipe(done);
    fflush(stdout);
    pid_t child = fork();
    if (child == 0) {
        close(done[0]);
        int f = in_capmode(d, getppid());
        fflush(stdout);
        write(done[1], f == 0 ? "D" : "F", 1);
        _exit(f == 0 ? 0 : 1);
    }
    close(done[1]);
    char verdict = 0;
    read(done[0], &verdict, 1);
    close(done[0]);
    int status = 0;
    waitpid(child, &status, 0);
    CHECK(verdict == 'D' && WIFEXITED(status) && WEXITSTATUS(status) == 0, "the child ran to the end and its checks passed ('%c', status %#x)", verdict ? verdict : '0', status);

    printf("the parent is untouched\n");
    CHECK(getmode() == 0, "cap_getmode is still 0 here");
    CHECK(!strcmp(slurp(sc(SYS_open, (long)"/tmp/cmout", O_RDONLY, 0)), "outside"), "open(/tmp/cmout) works here");
    CHECK(rmdir("/tmp/cmnew") != 0, "and the child's mkdirat(../cmnew) created nothing");

    printf("a thread's cap_enter covers the process\n");
    pid_t t = fork();
    if (t == 0) {
        pthread_t th;
        pthread_create(&th, NULL, thread_enters, NULL);
        pthread_join(th, NULL);
        _exit(getmode() == 1 && sc(SYS_open, (long)"/tmp/cmout", O_RDONLY, 0) == -ECAPMODE ? 42 : 43);
    }
    waitpid(t, &status, 0);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 42, "the main thread is in capability mode after its sibling entered (status %#x)", status);

    printf("every refusal was recorded (/proc/capdenials)\n");
    static char log[16384];
    long fd = sc(SYS_open, (long)"/proc/capdenials", O_RDONLY, 0);
    long n = 0, r;
    while (fd >= 0 && (r = sc(SYS_read, fd, (long)log + n, sizeof log - 1 - n)) > 0) n += r;
    if (fd >= 0) sc(SYS_close, fd, 0, 0);
    log[n] = 0;
    CHECK(count_lines_with(log, child, "path '/tmp/cmout' from /") >= 1, "open(/tmp/cmout) is named");
    CHECK(count_lines_with(log, child, "Open: path '/tmp/cmout' from /") >= 1, "open(2) itself names its path (musl's open)");
    CHECK(count_lines_with(log, child, "Stat: path '/etc' from /") >= 1, "stat(2) names its path");
    CHECK(count_lines_with(log, child, "path 'f' from the cwd") >= 1, "openat(AT_FDCWD, f) is named");
    CHECK(count_lines_with(log, child, "'../cmout' leaves directory fd") >= 1, "the .. escape is named");
    CHECK(count_lines_with(log, child, "'abs' leaves directory fd") >= 2, "the symlink escape is named (open and fstatat)");
    CHECK(count_lines_with(log, child, "Kill: pid") >= 1, "kill of the parent is named");
    CHECK(count_lines_with(log, child, "Connect: a global namespace") >= 1, "connect is named");
    CHECK(count_lines_with(log, child, "Sendto: a destination address") >= 1, "sendto is named");
    CHECK(count_lines_with(log, child, "Exec:") >= 1, "exec is named");
    // Exactly the child's 23 refusals: 7 paths from / or the cwd (open, 4 openat, stat, openat2), 7 leaving the dirfd
    // (5 openat/openat2, fstatat, mkdirat), kill, pidfd_open, connect, bind, sendto, chdir, getcwd, execve, setuid. openat2's own
    // EXDEV under RESOLVE_BENEATH is that call's answer, not a refusal. More would mean something allowed was refused.
    CHECK(count_lines_with(log, child, "ECAPMODE") + count_lines_with(log, child, "ENOTCAPABLE") == 23, "%d records for the child",
          count_lines_with(log, child, "ECAPMODE") + count_lines_with(log, child, "ENOTCAPABLE"));

    close((int)d);
    printf(failures ? "capmode_test: FAIL\n" : "capmode_test: PASS\n");
    return failures != 0;
}
