// setuid/setgid/setreuid/setresuid/setfsuid/getgroups/setgroups: Linux's rules for who may change what (nothing else in the
// kernel enforces an id). Each scenario runs in a forked child, since dropping root cannot be undone. Raw syscalls: the point is
// the kernel's ABI, not the libc wrapper (which, in musl, also broadcasts to every thread).
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <stdint.h>
#include <sys/wait.h>
#include <grp.h>
#include <errno.h>

static long sc(long nr, long a, long b, long c) {
    long r;
    __asm__ volatile("syscall" : "=a"(r) : "a"(nr), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return r;
}
enum { GETUID = 102, GETGID = 104, SETUID = 105, SETGID = 106, GETEUID = 107, GETEGID = 108, SETREUID = 113, SETREGID = 114,
       GETGROUPS = 115, SETGROUPS = 116, SETRESUID = 117, GETRESUID = 118, SETRESGID = 119, GETRESGID = 120, SETFSUID = 122,
       SETFSGID = 123 };
#define KEEP (-1L)
#define EPERM_ (-1L)
#define EINVAL_ (-22L)

static int failures;
#define CHECK(cond, ...) do { if (!(cond)) { failures++; printf("  FAIL %s (line %d): ", #cond, __LINE__); printf(__VA_ARGS__); printf("\n"); } } while (0)

static void ids(uint32_t *r, uint32_t *e, uint32_t *s) { sc(GETRESUID, (long)r, (long)e, (long)s); }
static void gids(uint32_t *r, uint32_t *e, uint32_t *s) { sc(GETRESGID, (long)r, (long)e, (long)s); }

// Run `fn` in a child; its failure count becomes the exit status.
static void in_child(const char *name, int (*fn)(void)) {
    pid_t p = fork();
    if (p == 0) { int f = fn(); _exit(f > 100 ? 100 : f); }
    int st = 0;
    waitpid(p, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) { failures++; printf("  FAIL scenario %s (status %#x)\n", name, st); }
    else printf("  ok   %s\n", name);
}

static int local_fail;
#define LCHECK(cond, ...) do { if (!(cond)) { local_fail++; printf("  FAIL %s (line %d): ", #cond, __LINE__); printf(__VA_ARGS__); printf("\n"); } } while (0)

static int s_root_defaults(void) {
    uint32_t r, e, s;
    ids(&r, &e, &s); LCHECK(r == 0 && e == 0 && s == 0, "uids %u %u %u", r, e, s);
    gids(&r, &e, &s); LCHECK(r == 0 && e == 0 && s == 0, "gids %u %u %u", r, e, s);
    LCHECK(sc(GETUID, 0, 0, 0) == 0 && sc(GETEUID, 0, 0, 0) == 0 && sc(GETGID, 0, 0, 0) == 0 && sc(GETEGID, 0, 0, 0) == 0, "get*");
    LCHECK(sc(GETGROUPS, 0, 0, 0) == 0, "no supplementary groups at boot");
    return local_fail;
}

static int s_setresuid(void) {
    uint32_t r, e, s;
    LCHECK(sc(SETRESUID, 1000, 1001, 1002) == 0, "root may set all three");
    ids(&r, &e, &s); LCHECK(r == 1000 && e == 1001 && s == 1002, "got %u %u %u", r, e, s);
    LCHECK(sc(GETUID, 0, 0, 0) == 1000 && sc(GETEUID, 0, 0, 0) == 1001, "getuid/geteuid follow");
    // No longer root: only the current three values are allowed.
    LCHECK(sc(SETRESUID, 7, KEEP, KEEP) == EPERM_, "unprivileged: foreign ruid refused");
    LCHECK(sc(SETRESUID, KEEP, 7, KEEP) == EPERM_, "unprivileged: foreign euid refused");
    LCHECK(sc(SETRESUID, KEEP, KEEP, 7) == EPERM_, "unprivileged: foreign suid refused");
    LCHECK(sc(SETRESUID, KEEP, 1002, 1000) == 0, "unprivileged: rotating known ids is fine");
    ids(&r, &e, &s); LCHECK(r == 1000 && e == 1002 && s == 1000, "got %u %u %u", r, e, s);
    LCHECK(sc(SETRESUID, 0, 0, 0) == EPERM_, "cannot come back to root");
    ids(&r, &e, &s); LCHECK(r == 1000 && e == 1002 && s == 1000, "a refused call changes nothing: %u %u %u", r, e, s);
    return local_fail;
}

static int s_setuid(void) {
    uint32_t r, e, s;
    LCHECK(sc(SETUID, 1000, 0, 0) == 0, "root setuid");
    ids(&r, &e, &s); LCHECK(r == 1000 && e == 1000 && s == 1000, "root setuid sets all three: %u %u %u", r, e, s);
    LCHECK(sc(SETUID, 0, 0, 0) == EPERM_, "dropped for good");
    return local_fail;
}

static int s_setuid_unprivileged(void) {
    uint32_t r, e, s;
    sc(SETRESUID, 1000, 1001, 1002);
    LCHECK(sc(SETUID, 5, 0, 0) == EPERM_, "unprivileged setuid to a foreign id");
    LCHECK(sc(SETUID, 1002, 0, 0) == 0, "setuid to the saved id sets only euid");
    ids(&r, &e, &s); LCHECK(r == 1000 && e == 1002 && s == 1002 - 0, "got %u %u %u", r, e, s);
    return local_fail;
}

static int s_setreuid(void) {
    uint32_t r, e, s;
    LCHECK(sc(SETREUID, 500, 600, 0) == 0, "root setreuid");
    ids(&r, &e, &s); LCHECK(r == 500 && e == 600 && s == 600, "saved follows effective when ruid is set: %u %u %u", r, e, s);
    LCHECK(sc(SETREUID, 600, 500, 0) == 0, "swap real and effective");
    ids(&r, &e, &s); LCHECK(r == 600 && e == 500 && s == 500, "swap: %u %u %u", r, e, s);
    LCHECK(sc(SETREUID, 9, KEEP, 0) == EPERM_, "unprivileged foreign ruid");
    LCHECK(sc(SETREUID, KEEP, 9, 0) == EPERM_, "unprivileged foreign euid");
    LCHECK(sc(SETREUID, KEEP, 600, 0) == 0, "euid back to the real id");
    ids(&r, &e, &s); LCHECK(r == 600 && e == 600, "%u %u %u", r, e, s);
    return local_fail;
}

static int s_setfsuid(void) {
    LCHECK(sc(SETFSUID, 42, 0, 0) == 0, "returns the previous fsuid (root's 0)");
    LCHECK(sc(SETFSUID, 43, 0, 0) == 42, "and the one before that");
    sc(SETRESUID, 1000, 1000, 1000);
    LCHECK(sc(SETFSUID, 1000, 0, 0) == 1000, "fsuid follows euid after setresuid");
    LCHECK(sc(SETFSUID, 9, 0, 0) == 1000, "unprivileged foreign fsuid: previous value returned");
    LCHECK(sc(SETFSUID, 1000, 0, 0) == 1000, "...and it did not change");
    return local_fail;
}

static int s_gids(void) {
    uint32_t r, e, s;
    LCHECK(sc(SETRESGID, 100, 101, 102) == 0, "root setresgid");
    gids(&r, &e, &s); LCHECK(r == 100 && e == 101 && s == 102, "%u %u %u", r, e, s);
    LCHECK(sc(GETGID, 0, 0, 0) == 100 && sc(GETEGID, 0, 0, 0) == 101, "getgid/getegid");
    // Group changes need root (euid 0), independent of the gids.
    LCHECK(sc(SETGID, 100, 0, 0) == 0, "root setgid");
    gids(&r, &e, &s); LCHECK(r == 100 && e == 100 && s == 100, "%u %u %u", r, e, s);
    LCHECK(sc(SETREGID, 5, 6, 0) == 0, "root setregid");
    gids(&r, &e, &s); LCHECK(r == 5 && e == 6 && s == 6, "%u %u %u", r, e, s);
    LCHECK(sc(SETFSGID, 77, 0, 0) == 6, "setfsgid returns the previous value");
    sc(SETUID, 1000, 0, 0);
    LCHECK(sc(SETGID, 99, 0, 0) == EPERM_, "unprivileged setgid to a foreign id");
    LCHECK(sc(SETGID, 5, 0, 0) == 0, "unprivileged setgid to the real gid");
    gids(&r, &e, &s); LCHECK(r == 5 && e == 5 && s == 6, "%u %u %u", r, e, s);
    return local_fail;
}

static int s_groups(void) {
    uint32_t g[4] = {10, 20, 30, 0};
    LCHECK(sc(SETGROUPS, 3, (long)g, 0) == 0, "root setgroups");
    LCHECK(sc(GETGROUPS, 0, 0, 0) == 3, "count");
    uint32_t out[4] = {0};
    LCHECK(sc(GETGROUPS, 2, (long)out, 0) == EINVAL_, "list too small");
    LCHECK(sc(GETGROUPS, 4, (long)out, 0) == 3 && out[0] == 10 && out[1] == 20 && out[2] == 30, "list %u %u %u", out[0], out[1], out[2]);
    LCHECK(sc(SETGROUPS, 1, 0, 0) < 0, "a NULL list with a count is a fault");
    LCHECK(sc(GETGROUPS, 0, 0, 0) == 3, "a refused call changes nothing");
    LCHECK(sc(SETGROUPS, 0, 0, 0) == 0 && sc(GETGROUPS, 0, 0, 0) == 0, "empty list clears");
    sc(SETGROUPS, 3, (long)g, 0);
    sc(SETUID, 1000, 0, 0);
    LCHECK(sc(SETGROUPS, 1, (long)g, 0) == EPERM_, "unprivileged setgroups");
    LCHECK(sc(GETGROUPS, 0, 0, 0) == 3, "unprivileged getgroups still works");
    return local_fail;
}

// The libc wrappers (mlibc's sysdeps) reach the kernel's ids instead of answering 0.
static int s_libc(void) {
    LCHECK(getuid() == 0 && geteuid() == 0 && getgid() == 0 && getegid() == 0, "root at start");
    gid_t g[3] = {11, 22, 33};
    LCHECK(setgroups(3, g) == 0, "setgroups");
    gid_t out[8];
    LCHECK(getgroups(0, NULL) == 3 && getgroups(8, out) == 3 && out[0] == 11 && out[2] == 33, "getgroups returns them");
    LCHECK(setgid(200) == 0 && getgid() == 200 && getegid() == 200, "setgid");
    LCHECK(setuid(300) == 0 && getuid() == 300 && geteuid() == 300, "setuid");
    uid_t r, e, s;
    LCHECK(getresuid(&r, &e, &s) == 0 && r == 300 && e == 300 && s == 300, "getresuid %u %u %u", r, e, s);
    LCHECK(setuid(0) == -1 && errno == EPERM, "no way back to root");
    LCHECK(seteuid(300) == 0 && setegid(200) == 0, "seteuid/setegid to the current ids");
    LCHECK(setreuid(-1, 5) == -1 && errno == EPERM, "setreuid to a foreign euid");
    LCHECK(setgroups(1, g) == -1 && errno == EPERM, "setgroups is root-only");
    return local_fail;
}

// Ids survive fork (inherited) and exec (kept). The exec'd copy checks, via argv[1].
static int s_inherit(void) {
    uint32_t g[2] = {7, 8};
    sc(SETGROUPS, 2, (long)g, 0);
    sc(SETRESGID, 21, 22, 23);
    sc(SETRESUID, 1000, 1001, 1002);
    pid_t p = fork();
    if (p == 0) {
        uint32_t r, e, s;
        ids(&r, &e, &s);
        int bad = !(r == 1000 && e == 1001 && s == 1002);
        gids(&r, &e, &s);
        bad |= !(r == 21 && e == 22 && s == 23);
        bad |= sc(GETGROUPS, 0, 0, 0) != 2;
        _exit(bad ? 11 : 0);
    }
    int st = 0; waitpid(p, &st, 0);
    LCHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "fork keeps ids, gids and groups (status %#x)", st);
    p = fork();
    if (p == 0) {
        char *argv[] = {"/mnt/bin/creds_test", "--check-exec", NULL};
        execv(argv[0], argv);
        _exit(99);
    }
    waitpid(p, &st, 0);
    LCHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "exec keeps ids, gids and groups (status %#x)", st);
    return local_fail;
}

static int check_exec(void) {
    uint32_t r, e, s;
    ids(&r, &e, &s);
    if (!(r == 1000 && e == 1001 && s == 1002)) return 21;
    gids(&r, &e, &s);
    if (!(r == 21 && e == 22 && s == 23)) return 22;
    if (sc(GETGROUPS, 0, 0, 0) != 2) return 23;
    return 0;
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "--check-exec")) return check_exec();
    printf("creds_test:\n");
    setvbuf(stdout, NULL, _IOLBF, 0);
    in_child("root defaults", s_root_defaults);
    in_child("setresuid", s_setresuid);
    in_child("setuid (root drops for good)", s_setuid);
    in_child("setuid (unprivileged)", s_setuid_unprivileged);
    in_child("setreuid", s_setreuid);
    in_child("setfsuid", s_setfsuid);
    in_child("gids", s_gids);
    in_child("groups", s_groups);
    in_child("inherit across fork and exec", s_inherit);
    in_child("libc wrappers", s_libc);
    CHECK(sc(GETUID, 0, 0, 0) == 0, "the parent was never touched");
    printf(failures ? "creds_test: %d FAILURES\n" : "creds_test: OK\n", failures);
    return failures ? 1 : 0;
}
