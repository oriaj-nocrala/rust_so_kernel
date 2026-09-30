// The *at family relative to a directory fd (openat 257, mkdirat 258, newfstatat 262, unlinkat 263, renameat 264/renameat2 316,
// symlinkat 266, readlinkat 267, fchmodat 268, faccessat 269/439, utimensat 280) and fchdir 81 — what Rust's std uses for
// remove_dir_all and what libc's *at wrappers call. A recursive delete like std's runs at the end.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/stat.h>

#define printf(...) ((printf)(__VA_ARGS__), fflush(stdout))
static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

static long sc(long nr, long a, long b, long c, long d, long e) {
    long r;
    register long r10 __asm__("r10") = d;
    register long r8 __asm__("r8") = e;
    __asm__ volatile("syscall" : "=a"(r) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8) : "rcx", "r11", "memory");
    return r;
}
enum { AT_FDCWD_ = -100, O_RDONLY_ = 0, O_WRONLY_ = 1, O_CREAT_ = 0x40, O_TRUNC_ = 0x200, O_DIRECTORY_ = 0x10000 };
#define AT_SYMLINK_NOFOLLOW_ 0x100
#define AT_REMOVEDIR_ 0x200
#define AT_EMPTY_PATH_ 0x1000

static long openat_(long dfd, const char *p, long fl) { return sc(257, dfd, (long)p, fl, 0644, 0); }
static long mkdirat_(long dfd, const char *p) { return sc(258, dfd, (long)p, 0755, 0, 0); }
static long unlinkat_(long dfd, const char *p, long fl) { return sc(263, dfd, (long)p, fl, 0, 0); }
static long fstatat_(long dfd, const char *p, void *st, long fl) { return sc(262, dfd, (long)p, (long)st, fl, 0); }
static uint64_t st_size(const unsigned char *st) { uint64_t v; memcpy(&v, st + 48, 8); return v; }
static uint32_t st_mode(const unsigned char *st) { uint32_t v; memcpy(&v, st + 24, 4); return v; }

// getdents64 on an open directory: each name, skipping . and ..
static int list(long dfd, char names[][64], int max) {
    char buf[2048];
    int n = 0;
    for (;;) {
        long r = sc(217, dfd, (long)buf, sizeof buf, 0, 0);
        if (r <= 0) break;
        for (long off = 0; off < r;) {
            uint16_t reclen; memcpy(&reclen, buf + off + 16, 2);
            const char *name = buf + off + 19;
            if (strcmp(name, ".") && strcmp(name, "..") && n < max) { strncpy(names[n], name, 63); names[n][63] = 0; n++; }
            off += reclen;
        }
    }
    return n;
}

// std's remove_dir_all in miniature: open the directory relative to its parent, list it, unlinkat each entry, recurse into
// directories (told apart by fstatat), then rmdir it through the parent.
static int remove_tree(long parent, const char *name) {
    long d = openat_(parent, name, O_RDONLY_ | O_DIRECTORY_);
    if (d < 0) return (int)d;
    static char names[16][64];
    char local[16][64];
    int n = list(d, names, 16);
    memcpy(local, names, sizeof local);
    for (int i = 0; i < n; i++) {
        unsigned char st[144];
        if (fstatat_(d, local[i], st, AT_SYMLINK_NOFOLLOW_) != 0) return -1;
        if ((st_mode(st) & 0170000) == 0040000) {
            int r = remove_tree(d, local[i]);
            if (r) return r;
        } else if (unlinkat_(d, local[i], 0) != 0) return -2;
    }
    close((int)d);
    return (int)unlinkat_(parent, name, AT_REMOVEDIR_);
}

int main(void) {
    rmdir("/tmp/at_t");
    mkdir("/tmp/at_t", 0755);
    long d = openat_(AT_FDCWD_, "/tmp/at_t", O_RDONLY_ | O_DIRECTORY_);
    CHECK(d >= 0, "opening the directory gave %ld", d);

    printf("create, write, read back relative to the directory fd\n");
    long f = openat_(d, "f", O_CREAT_ | O_WRONLY_ | O_TRUNC_);
    CHECK(f >= 0, "openat(d, f, O_CREAT) gave %ld", f);
    CHECK(write((int)f, "hello", 5) == 5, "write");
    close((int)f);
    char buf[16] = {0};
    f = openat_(d, "f", O_RDONLY_);
    CHECK(f >= 0 && read((int)f, buf, 15) == 5 && memcmp(buf, "hello", 5) == 0, "read back '%s'", buf);
    close((int)f);
    CHECK(access("/tmp/at_t/f", 0) == 0, "the file is at /tmp/at_t/f, not in the cwd");
    CHECK(openat_(d, "missing", O_RDONLY_) == -ENOENT, "a missing file gave %ld", openat_(d, "missing", O_RDONLY_));

    printf("mkdirat, newfstatat, AT_EMPTY_PATH\n");
    CHECK(mkdirat_(d, "sub") == 0, "mkdirat");
    CHECK(mkdirat_(d, "sub") == -EEXIST, "mkdirat of an existing name gave %ld", mkdirat_(d, "sub"));
    unsigned char st[144];
    CHECK(fstatat_(d, "f", st, 0) == 0 && st_size(st) == 5 && (st_mode(st) & 0170000) == 0100000, "stat of f: size %llu mode %o", (unsigned long long)st_size(st), st_mode(st));
    CHECK(fstatat_(d, "sub", st, 0) == 0 && (st_mode(st) & 0170000) == 0040000, "stat of sub is not a directory: mode %o", st_mode(st));
    CHECK(fstatat_(d, "nope", st, 0) == -ENOENT, "stat of a missing name");
    CHECK(fstatat_(d, "", st, AT_EMPTY_PATH_) == 0 && (st_mode(st) & 0170000) == 0040000, "AT_EMPTY_PATH stats the fd itself: mode %o", st_mode(st));
    CHECK(fstatat_(d, "f", st, 0x40000000) == -EINVAL, "an unknown flag");

    printf("symlinkat, readlinkat\n");
    CHECK(sc(266, (long)"f", d, (long)"lnk", 0, 0) == 0, "symlinkat");
    char tgt[16] = {0};
    long n = sc(267, d, (long)"lnk", (long)tgt, 15, 0);
    CHECK(n == 1 && tgt[0] == 'f', "readlinkat gave %ld '%s'", n, tgt);
    CHECK(fstatat_(d, "lnk", st, AT_SYMLINK_NOFOLLOW_) == 0 && (st_mode(st) & 0170000) == 0120000, "lstat of the link: mode %o", st_mode(st));
    CHECK(fstatat_(d, "lnk", st, 0) == 0 && st_size(st) == 5, "stat follows the link: size %llu", (unsigned long long)st_size(st));

    printf("renameat / renameat2, faccessat / faccessat2, fchmodat, utimensat\n");
    CHECK(sc(264, d, (long)"f", d, (long)"g", 0) == 0, "renameat");
    CHECK(fstatat_(d, "f", st, 0) == -ENOENT && fstatat_(d, "g", st, 0) == 0, "f is gone and g is there");
    CHECK(sc(316, d, (long)"g", d, (long)"h", 0) == 0, "renameat2 with flags 0");
    CHECK(sc(316, d, (long)"h", d, (long)"g", 1) == -EINVAL, "renameat2 with RENAME_NOREPLACE gave %ld", sc(316, d, (long)"h", d, (long)"g", 1));
    CHECK(sc(264, d, (long)"h", d, (long)"sub/moved", 0) == 0, "rename into a subdirectory");
    CHECK(sc(269, d, (long)"sub/moved", 0, 0, 0) == 0, "faccessat");
    CHECK(sc(439, d, (long)"sub/moved", 0, 0, 0) == 0, "faccessat2");
    CHECK(sc(269, d, (long)"nope", 0, 0, 0) == -ENOENT, "faccessat of a missing name");
    CHECK(sc(268, d, (long)"sub/moved", 0600, 0, 0) == 0, "fchmodat gave %ld", sc(268, d, (long)"sub/moved", 0600, 0, 0));
    CHECK(sc(280, d, (long)"sub/moved", 0, 0, 0) == 0, "utimensat relative to a dirfd gave %ld", sc(280, d, (long)"sub/moved", 0, 0, 0));

    printf("dirfd errors; absolute paths ignore the dirfd; .. through the dirfd\n");
    CHECK(openat_(999, "x", O_RDONLY_) == -EBADF, "an fd that is not open gave %ld", openat_(999, "x", O_RDONLY_));
    int p[2]; pipe(p);
    CHECK(openat_(p[0], "x", O_RDONLY_) == -ENOTDIR, "a pipe as dirfd gave %ld", openat_(p[0], "x", O_RDONLY_));
    long any = openat_(999, "/tmp/at_t/sub/moved", O_RDONLY_);
    CHECK(any >= 0, "an absolute path with a bad dirfd gave %ld", any);
    if (any >= 0) close((int)any);
    long sd = openat_(d, "sub", O_RDONLY_ | O_DIRECTORY_);
    long up = openat_(sd, "../g", O_RDONLY_);
    CHECK(sd >= 0 && up == -ENOENT, "../g from sub gave %ld (g was renamed away)", up);
    long via = openat_(sd, "moved", O_RDONLY_);
    CHECK(via >= 0, "a file through the sub directory fd gave %ld", via);
    if (via >= 0) close((int)via);

    printf("getdents64 on the fd, fchdir\n");
    static char names[16][64];
    long fresh = openat_(d, ".", O_RDONLY_ | O_DIRECTORY_);   // a listing is a snapshot from open time (POSIX leaves later changes unspecified)
    int count = list(fresh, names, 16);
    close((int)fresh);
    int has_sub = 0, has_lnk = 0;
    for (int i = 0; i < count; i++) { has_sub |= !strcmp(names[i], "sub"); has_lnk |= !strcmp(names[i], "lnk"); }
    CHECK(count == 2 && has_sub && has_lnk, "the directory lists %d entries", count);
    CHECK(sc(81, d, 0, 0, 0, 0) == 0, "fchdir");
    char cwd[64] = {0};
    sc(79, (long)cwd, 64, 0, 0, 0);
    CHECK(strcmp(cwd, "/tmp/at_t") == 0, "cwd is '%s'", cwd);
    CHECK(access("sub/moved", 0) == 0, "a relative path now resolves from the fchdir'd directory");
    chdir("/");

    printf("unlinkat: files, AT_REMOVEDIR\n");
    CHECK(unlinkat_(d, "lnk", 0) == 0, "unlinkat of the symlink");
    CHECK(unlinkat_(d, "sub", 0) != 0, "unlinkat without AT_REMOVEDIR must not remove a directory");
    CHECK(unlinkat_(d, "sub", AT_REMOVEDIR_) != 0, "AT_REMOVEDIR of a non-empty directory must fail");
    CHECK(unlinkat_(d, "sub/moved", 0) == 0, "unlinkat of sub/moved");
    CHECK(unlinkat_(d, "sub", AT_REMOVEDIR_) == 0, "AT_REMOVEDIR of the now empty directory");
    CHECK(unlinkat_(d, "sub", AT_REMOVEDIR_) == -ENOENT, "removing it again gave %ld", unlinkat_(d, "sub", AT_REMOVEDIR_));
    CHECK(unlinkat_(d, "x", 0x1) == -EINVAL, "an unknown flag");

    printf("a recursive delete like std's remove_dir_all\n");
    mkdirat_(d, "tree"); mkdirat_(d, "tree/a"); mkdirat_(d, "tree/a/b"); mkdirat_(d, "tree/c");
    long t;
    t = openat_(d, "tree/a/b/file1", O_CREAT_ | O_WRONLY_); close((int)t);
    t = openat_(d, "tree/a/file2", O_CREAT_ | O_WRONLY_); close((int)t);
    t = openat_(d, "tree/top", O_CREAT_ | O_WRONLY_); close((int)t);
    sc(266, (long)"top", (long)openat_(d, "tree", O_RDONLY_ | O_DIRECTORY_), (long)"link-to-top", 0, 0);
    CHECK(remove_tree(d, "tree") == 0, "remove_tree returned %d", remove_tree(d, "tree"));
    CHECK(fstatat_(d, "tree", st, 0) == -ENOENT, "the tree is gone");

    close((int)d);
    CHECK(rmdir("/tmp/at_t") == 0, "the directory is empty again");
    printf(failures ? "at_test: FAIL\n" : "at_test: PASS\n");
    return failures != 0;
}
