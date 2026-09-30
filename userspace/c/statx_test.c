// statx(332): the same facts as newfstatat, in struct statx's layout; AT_EMPTY_PATH, AT_SYMLINK_NOFOLLOW, dirfd-relative paths,
// the reserved-mask and unknown-flag errors, and a mask with no birth time. Raw syscalls: the point is the kernel's ABI.
#include <stdio.h>
#include <string.h>
#include <stdint.h>
#include <unistd.h>
#include <fcntl.h>

static long sc(long nr, long a, long b, long c, long d, long e) {
    long r;
    register long r10 __asm__("r10") = d;
    register long r8 __asm__("r8") = e;
    __asm__ volatile("syscall" : "=a"(r) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8) : "rcx", "r11", "memory");
    return r;
}
static int failures;
#define CHECK(cond, ...) do { if (!(cond)) { failures++; printf("  FAIL %s (line %d): ", #cond, __LINE__); printf(__VA_ARGS__); printf("\n"); } } while (0)

#define AT_FDCWD_ (-100)
#define AT_SYMLINK_NOFOLLOW_ 0x100
#define AT_EMPTY_PATH_ 0x1000
#define EINVAL_ (-22L)
#define ENOENT_ (-2L)
#define EBADF_ (-9L)

static uint64_t u64(const unsigned char *p, int off) { uint64_t v; memcpy(&v, p + off, 8); return v; }
static uint32_t u32(const unsigned char *p, int off) { uint32_t v; memcpy(&v, p + off, 4); return v; }
static uint16_t u16(const unsigned char *p, int off) { uint16_t v; memcpy(&v, p + off, 2); return v; }

// newfstatat's struct stat offsets (x86-64 Linux).
static long nfs(long dfd, const char *p, unsigned char *st, long fl) { return sc(262, dfd, (long)p, (long)st, fl, 0); }
static long sx(long dfd, const char *p, long fl, long mask, unsigned char *buf) { return sc(332, dfd, (long)p, fl, mask, (long)buf); }

static void same_as_stat(const unsigned char *x, const unsigned char *st, const char *what) {
    CHECK(u32(x, 16) == (uint32_t)u64(st, 16), "%s: nlink %u vs %lu", what, u32(x, 16), (unsigned long)u64(st, 16));
    CHECK(u16(x, 28) == (uint16_t)u32(st, 24), "%s: mode %o vs %o", what, u16(x, 28), u32(st, 24));
    CHECK(u32(x, 20) == u32(st, 28) && u32(x, 24) == u32(st, 32), "%s: uid/gid", what);
    CHECK(u64(x, 32) == u64(st, 8), "%s: ino", what);
    CHECK(u64(x, 40) == u64(st, 48), "%s: size %lu vs %lu", what, (unsigned long)u64(x, 40), (unsigned long)u64(st, 48));
    CHECK(u64(x, 48) == u64(st, 64), "%s: blocks", what);
    CHECK(u32(x, 4) == (uint32_t)u64(st, 56), "%s: blksize", what);
    CHECK(u64(x, 112) == u64(st, 88), "%s: mtime", what);
    CHECK(u64(x, 64) == u64(st, 72), "%s: atime", what);
    CHECK(u64(x, 96) == u64(st, 104), "%s: ctime", what);
    CHECK((u32(x, 0) & 0x7ff) == 0x7ff, "%s: basic stats in the returned mask (%#x)", what, u32(x, 0));
    CHECK((u32(x, 0) & 0x800) == 0, "%s: no birth time claimed", what);
}

int main(void) {
    unsigned char x[256], st[144];
    printf("statx_test:\n");

    // A regular file with a known size.
    unlink("/tmp/sx_file"); unlink("/tmp/sx_link");
    int fd = open("/tmp/sx_file", O_CREAT | O_WRONLY | O_TRUNC, 0644);
    CHECK(fd >= 0, "create");
    CHECK(write(fd, "hello, statx", 12) == 12, "write");
    memset(x, 0xAA, sizeof x);
    CHECK(sx(AT_FDCWD_, "/tmp/sx_file", 0, 0x7ff, x) == 0, "statx path");
    CHECK(nfs(AT_FDCWD_, "/tmp/sx_file", st, 0) == 0, "stat path");
    same_as_stat(x, st, "path");
    CHECK(u64(x, 40) == 12, "size is 12 (%lu)", (unsigned long)u64(x, 40));
    CHECK((u16(x, 28) & 0170000) == 0100000, "S_IFREG");
    CHECK(x[255] == 0 && u64(x, 56) == 0, "padding and attributes_mask are zeroed, not stale stack bytes");
    CHECK(u32(x, 12 + 0) == 0 && u32(x, 8) == 0, "no attributes reported");

    // AT_EMPTY_PATH on an open fd.
    memset(x, 0xAA, sizeof x);
    CHECK(sx(fd, "", AT_EMPTY_PATH_, 0x7ff, x) == 0, "statx fd");
    CHECK(u64(x, 40) == 12 && u64(x, 32) == u64(st, 8), "fd: size and inode of the open file");
    CHECK(sx(fd, "", 0, 0x7ff, x) == ENOENT_, "empty path without AT_EMPTY_PATH is ENOENT");
    CHECK(sx(999, "", AT_EMPTY_PATH_, 0x7ff, x) == EBADF_, "bad fd");
    close(fd);

    // Symlink: NOFOLLOW reports the link, the default follows it.
    CHECK(symlink("/tmp/sx_file", "/tmp/sx_link") == 0, "symlink");
    CHECK(sx(AT_FDCWD_, "/tmp/sx_link", AT_SYMLINK_NOFOLLOW_, 0x7ff, x) == 0, "statx nofollow");
    CHECK((u16(x, 28) & 0170000) == 0120000, "nofollow: S_IFLNK (%o)", u16(x, 28));
    CHECK(u64(x, 40) == strlen("/tmp/sx_file"), "nofollow: size is the target length (%lu)", (unsigned long)u64(x, 40));
    CHECK(sx(AT_FDCWD_, "/tmp/sx_link", 0, 0x7ff, x) == 0, "statx follow");
    CHECK((u16(x, 28) & 0170000) == 0100000 && u64(x, 40) == 12, "follow: the file behind it");

    // dirfd-relative.
    int dfd = open("/tmp", O_RDONLY | O_DIRECTORY);
    CHECK(dfd >= 0, "open /tmp");
    CHECK(sx(dfd, "sx_file", 0, 0x7ff, x) == 0 && u64(x, 40) == 12, "relative to a dirfd");
    CHECK((sx(dfd, "", AT_EMPTY_PATH_, 0x7ff, x) == 0) && (u16(x, 28) & 0170000) == 0040000, "the directory itself is S_IFDIR");
    close(dfd);

    // A device: rdev and dev decode like major()/minor().
    CHECK(nfs(AT_FDCWD_, "/dev/null", st, 0) == 0 && sx(AT_FDCWD_, "/dev/null", 0, 0x7ff, x) == 0, "/dev/null");
    uint64_t rd = u64(st, 40);
    CHECK(u32(x, 128) == (uint32_t)(((rd >> 8) & 0xfff) | ((rd >> 32) & ~0xfffUL)) && u32(x, 132) == (uint32_t)((rd & 0xff) | ((rd >> 12) & ~0xffUL)), "rdev major/minor of /dev/null");
    CHECK((u16(x, 28) & 0170000) == 0020000, "/dev/null is a character device (%o)", u16(x, 28));

    // Errors.
    CHECK(sx(AT_FDCWD_, "/tmp/nope", 0, 0x7ff, x) == ENOENT_, "missing file");
    CHECK(sx(AT_FDCWD_, "/tmp/sx_file", 0x40000000, 0x7ff, x) == EINVAL_, "unknown flag");
    CHECK(sx(AT_FDCWD_, "/tmp/sx_file", 0, 0x80000000L, x) == EINVAL_, "reserved mask bit");
    CHECK(sx(AT_FDCWD_, "/tmp/sx_file", 0, 0x7ff, (unsigned char *)0) < 0, "NULL buffer fails");
    // A mask asking for less still succeeds (extra fields are allowed).
    CHECK(sx(AT_FDCWD_, "/tmp/sx_file", 0, 0x1 /* STATX_TYPE */, x) == 0, "small mask");

    unlink("/tmp/sx_file"); unlink("/tmp/sx_link");
    printf(failures ? "statx_test: %d FAILURES\n" : "statx_test: OK\n", failures);
    return failures ? 1 : 0;
}
