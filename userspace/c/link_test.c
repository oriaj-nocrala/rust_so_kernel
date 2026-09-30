// link/linkat: a second name for the same file on ramfs (/tmp) and ext2 (/mnt): same inode, st_nlink counts names, data shared,
// blocks freed only with the last name. Errors: directory (EPERM), taken name (EEXIST), missing source (ENOENT), other
// filesystem (EXDEV). A final symlink is linked itself unless AT_SYMLINK_FOLLOW.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/stat.h>
#include <sys/statvfs.h>

static int failures;
#define CHECK(cond, ...) do { if (!(cond)) { failures++; printf("  FAIL %s (line %d): ", #cond, __LINE__); printf(__VA_ARGS__); printf(" [errno %d]\n", errno); } } while (0)

static long sc(long nr, long a, long b, long c, long d, long e) {
    long r;
    register long r10 __asm__("r10") = d;
    register long r8 __asm__("r8") = e;
    __asm__ volatile("syscall" : "=a"(r) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8) : "rcx", "r11", "memory");
    return r;
}
#define AT_FDCWD_ (-100)
#define AT_SYMLINK_FOLLOW_ 0x400

static void put(const char *path, const char *text, size_t repeat) {
    int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    for (size_t i = 0; i < repeat; i++) write(fd, text, strlen(text));
    close(fd);
}
static long slurp(const char *path, char *buf, size_t max) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    long n = read(fd, buf, max - 1);
    close(fd);
    if (n >= 0) buf[n] = 0;
    return n;
}
// Free space in bytes (blocks are 1 KiB or 4 KiB depending on the image).
static long free_blocks(const char *path) { struct statvfs v; return statvfs(path, &v) == 0 ? (long)(v.f_bfree * v.f_frsize) : -1; }

static void basic(const char *dir) {
    char a[128], b[128], c[128], buf[64];
    snprintf(a, sizeof a, "%s/a", dir); snprintf(b, sizeof b, "%s/b", dir); snprintf(c, sizeof c, "%s/c", dir);
    struct stat sa, sb;
    printf(" [%s]\n", dir);
    put(a, "hello", 1);
    CHECK(stat(a, &sa) == 0 && sa.st_nlink == 1, "fresh file has one name (%lu)", (unsigned long)sa.st_nlink);
    CHECK(link(a, b) == 0, "link");
    CHECK(stat(a, &sa) == 0 && stat(b, &sb) == 0, "stat both");
    CHECK(sa.st_ino == sb.st_ino, "same inode %lu %lu", (unsigned long)sa.st_ino, (unsigned long)sb.st_ino);
    CHECK(sa.st_nlink == 2 && sb.st_nlink == 2, "nlink is 2 (%lu %lu)", (unsigned long)sa.st_nlink, (unsigned long)sb.st_nlink);
    int fd = open(a, O_RDONLY);
    struct stat sf;
    CHECK(fstat(fd, &sf) == 0 && sf.st_nlink == 2, "fstat sees nlink 2 (%lu)", (unsigned long)sf.st_nlink);
    // Data written through one name shows through the other.
    int w = open(b, O_WRONLY | O_APPEND);
    CHECK(write(w, " world", 6) == 6, "append via b");
    close(w);
    CHECK(slurp(a, buf, sizeof buf) == 11 && !strcmp(buf, "hello world"), "a shows b's write: '%s'", buf);
    // The open fd from before the link survives unlinking a name.
    CHECK(unlink(a) == 0, "unlink a");
    CHECK(stat(b, &sb) == 0 && sb.st_nlink == 1, "b now has one name (%lu)", (unsigned long)sb.st_nlink);
    CHECK(fstat(fd, &sf) == 0 && sf.st_nlink == 1, "the open fd sees it too (%lu)", (unsigned long)sf.st_nlink);
    close(fd);
    CHECK(slurp(b, buf, sizeof buf) == 11 && !strcmp(buf, "hello world"), "data intact under the remaining name");
    // Errors.
    CHECK(link(b, c) == 0 && link(b, c) == -1 && errno == EEXIST, "EEXIST on a taken name");
    CHECK(link("/nonexistent_link_src", a) == -1 && errno == ENOENT, "ENOENT on a missing source");
    char d[128]; snprintf(d, sizeof d, "%s/d", dir);
    mkdir(d, 0755);
    CHECK(link(d, a) == -1 && errno == EPERM, "EPERM linking a directory");
    CHECK(stat(a, &sa) == -1, "no name was created by the failed calls");
    rmdir(d);
    unlink(b); unlink(c);
}

static void symlinks(const char *dir) {
    char f[128], l[128], n1[128], n2[128];
    snprintf(f, sizeof f, "%s/file", dir); snprintf(l, sizeof l, "%s/sym", dir);
    snprintf(n1, sizeof n1, "%s/lnk_nofollow", dir); snprintf(n2, sizeof n2, "%s/lnk_follow", dir);
    put(f, "x", 1);
    symlink(f, l);
    struct stat s;
    CHECK(link(l, n1) == 0, "link(symlink)");
    CHECK(lstat(n1, &s) == 0 && S_ISLNK(s.st_mode), "the new name is the symlink itself, not its target (mode %o)", s.st_mode);
    CHECK(sc(265, AT_FDCWD_, (long)l, AT_FDCWD_, (long)n2, AT_SYMLINK_FOLLOW_) == 0, "linkat AT_SYMLINK_FOLLOW");
    CHECK(lstat(n2, &s) == 0 && S_ISREG(s.st_mode) && s.st_nlink == 2, "linked the target file (mode %o nlink %lu)", s.st_mode, (unsigned long)s.st_nlink);
    CHECK(sc(265, AT_FDCWD_, (long)l, AT_FDCWD_, (long)n2, 0x100) < 0, "an unsupported flag is refused");
    unlink(n1); unlink(n2); unlink(l); unlink(f);
}

static void dirfds(const char *dir) {
    char f[128];
    snprintf(f, sizeof f, "%s/rel_src", dir);
    put(f, "rel", 1);
    int dfd = open(dir, O_RDONLY | O_DIRECTORY);
    CHECK(sc(265, dfd, (long)"rel_src", dfd, (long)"rel_dst", 0) == 0, "linkat relative to a dirfd");
    struct stat a, b;
    char g[128]; snprintf(g, sizeof g, "%s/rel_dst", dir);
    CHECK(stat(f, &a) == 0 && stat(g, &b) == 0 && a.st_ino == b.st_ino, "relative link is the same inode");
    close(dfd);
    unlink(f); unlink(g);
}

int main(void) {
    printf("link_test:\n");
    mkdir("/mnt/link_test_dir", 0755);
    mkdir("/tmp/link_test_dir", 0755);
    // Leftovers of an earlier run that died midway.
    const char *dirs[] = {"/tmp/link_test_dir", "/mnt/link_test_dir"};
    const char *names[] = {"a", "b", "c", "d", "x", "y", "big", "big2", "file", "sym", "lnk_nofollow", "lnk_follow", "rel_src", "rel_dst"};
    for (int i = 0; i < 2; i++)
        for (unsigned j = 0; j < sizeof names / sizeof *names; j++) {
            char p[160]; snprintf(p, sizeof p, "%s/%s", dirs[i], names[j]);
            unlink(p); rmdir(p);
        }
    basic("/tmp/link_test_dir");
    basic("/mnt/link_test_dir");
    symlinks("/tmp/link_test_dir");
    symlinks("/mnt/link_test_dir");
    dirfds("/tmp/link_test_dir");
    dirfds("/mnt/link_test_dir");

    // Across filesystems.
    put("/tmp/link_test_dir/x", "x", 1);
    CHECK(link("/tmp/link_test_dir/x", "/mnt/link_test_dir/x") == -1 && errno == EXDEV, "ramfs -> ext2 is EXDEV");
    put("/mnt/link_test_dir/y", "y", 1);
    CHECK(link("/mnt/link_test_dir/y", "/tmp/link_test_dir/y") == -1 && errno == EXDEV, "ext2 -> ramfs is EXDEV");
    unlink("/tmp/link_test_dir/x"); unlink("/mnt/link_test_dir/y");

    // ext2: the blocks go with the last name, not the first.
    puts(" [ext2 blocks]");
    const char *p = "/mnt/link_test_dir/big", *q = "/mnt/link_test_dir/big2";
    sc(162, 0, 0, 0, 0, 0);
    long before = free_blocks("/mnt");
    put(p, "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", 2048);   // 128 KiB
    sc(162, 0, 0, 0, 0, 0);
    long during = free_blocks("/mnt");
    CHECK(before - during >= 131072, "the file took blocks (%ld -> %ld)", before, during);
    CHECK(link(p, q) == 0, "link big");
    CHECK(unlink(p) == 0, "unlink first name");
    sc(162, 0, 0, 0, 0, 0);
    CHECK(free_blocks("/mnt") == during, "blocks are still held by the second name (%ld vs %ld)", free_blocks("/mnt"), during);
    struct stat s;
    CHECK(stat(q, &s) == 0 && s.st_size == 131072 && s.st_nlink == 1, "size intact, nlink 1 (%ld, %lu)", (long)s.st_size, (unsigned long)s.st_nlink);
    CHECK(unlink(q) == 0, "unlink last name");
    sc(162, 0, 0, 0, 0, 0);
    CHECK(free_blocks("/mnt") >= before - 8192, "blocks are freed with the last name (%ld vs %ld)", free_blocks("/mnt"), before);

    rmdir("/mnt/link_test_dir");
    rmdir("/tmp/link_test_dir");
    printf(failures ? "link_test: %d FAILURES\n" : "link_test: OK\n", failures);
    return failures ? 1 : 0;
}
