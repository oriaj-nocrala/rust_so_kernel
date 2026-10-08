// openat2(437): the open_how checks (EINVAL/E2BIG), RESOLVE_BENEATH (no `..` above the dirfd, no absolute paths or
// absolute symlinks, no relative symlink that climbs out, `..` after a symlink is the physical parent, mounts under the
// bound) and RESOLVE_NO_SYMLINKS, O_CREAT/O_EXCL/O_NOFOLLOW through the bounded walk, and that an fd opened beneath
// records its real path for the next *at call. Raw syscalls: the point is the kernel's ABI.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <sys/stat.h>

static long sc4(long nr, long a, long b, long c, long d) {
    long ret;
    register long r10 __asm__("r10") = d;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10) : "rcx", "r11", "memory");
    return ret;
}

enum { SYS_openat = 257, SYS_openat2 = 437, AT_FDCWD_ = -100 };
enum { R_NO_XDEV = 1, R_NO_MAGICLINKS = 2, R_NO_SYMLINKS = 4, R_BENEATH = 8, R_IN_ROOT = 0x10 };

struct how_v0 { uint64_t flags, mode, resolve; };

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

static long o2(long dirfd, const char *path, uint64_t flags, uint64_t mode, uint64_t resolve) {
    struct how_v0 how = { flags, mode, resolve };
    return sc4(SYS_openat2, dirfd, (long)path, (long)&how, sizeof how);
}

// Read up to 31 bytes from `fd` into a static buffer, close it, return the text ("" on error).
static const char *slurp(long fd) {
    static char buf[32];
    if (fd < 0) return "";
    long n = read((int)fd, buf, sizeof buf - 1);
    close((int)fd);
    buf[n > 0 ? n : 0] = 0;
    return buf;
}

static void put(const char *path, const char *text) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    write(fd, text, strlen(text));
    close(fd);
}

int main(void) {
    // /tmp/o2/{f, sub/{g, deep/}} and /tmp/o2out, plus the symlinks the cases need.
    unlink("/tmp/o2/abs"); unlink("/tmp/o2/up"); unlink("/tmp/o2/sub/sib"); unlink("/tmp/o2/d");
    unlink("/tmp/o2/a"); unlink("/tmp/o2/b"); unlink("/tmp/o2/sub/new"); unlink("/tmp/o2new");
    mkdir("/tmp/o2", 0755); mkdir("/tmp/o2/sub", 0755); mkdir("/tmp/o2/sub/deep", 0755);
    put("/tmp/o2/f", "inside");
    put("/tmp/o2/sub/g", "gee");
    put("/tmp/o2out", "outside");
    symlink("/tmp/o2out", "/tmp/o2/abs");
    symlink("../o2out", "/tmp/o2/up");
    symlink("../f", "/tmp/o2/sub/sib");
    symlink("sub/deep", "/tmp/o2/d");
    symlink("b", "/tmp/o2/a");
    symlink("a", "/tmp/o2/b");
    long d = open("/tmp/o2", O_RDONLY | O_DIRECTORY);
    CHECK(d >= 0, "open /tmp/o2: %ld", d);

    printf("open_how checks\n");
    {
        struct how_v0 how = { O_RDONLY, 0, 0 };
        CHECK(sc4(SYS_openat2, d, (long)"f", (long)&how, 16) == -EINVAL, "size 16: EINVAL");
        uint8_t big[64] = {0};
        memcpy(big, &how, sizeof how);
        long fd = sc4(SYS_openat2, d, (long)"f", (long)big, sizeof big);
        CHECK(fd >= 0, "size 64 with a zero tail is accepted: %ld", fd);
        close((int)fd);
        big[63] = 1;
        CHECK(sc4(SYS_openat2, d, (long)"f", (long)big, sizeof big) == -E2BIG, "a nonzero tail: E2BIG");
        CHECK(sc4(SYS_openat2, d, (long)"f", (long)big, 8192) == -E2BIG, "size 8192: E2BIG");
        CHECK(sc4(SYS_openat2, d, (long)"f", 0, sizeof how) == -EFAULT, "how = NULL: EFAULT");
    }
    CHECK(o2(d, "f", 0, 0, 0x40) == -EINVAL, "unknown resolve bit: EINVAL");
    CHECK(o2(d, "f", 0, 0, R_BENEATH | R_IN_ROOT) == -EINVAL, "BENEATH|IN_ROOT: EINVAL");
    CHECK(o2(d, "f", 0, 0, R_NO_XDEV) == -EINVAL, "NO_XDEV (not implemented): EINVAL");
    CHECK(o2(d, "f", 0, 0, R_NO_MAGICLINKS) == -EINVAL, "NO_MAGICLINKS (not implemented): EINVAL");
    CHECK(o2(d, "f", 0, 0644, 0) == -EINVAL, "mode without O_CREAT: EINVAL");
    CHECK(o2(d, "f", O_CREAT, 010644, 0) == -EINVAL, "mode above 07777: EINVAL");
    CHECK(o2(d, "f", 1ul << 40, 0, 0) == -EINVAL, "unknown flag bit: EINVAL");
    CHECK(!strcmp(slurp(o2(d, "../o2out", O_RDONLY, 0, 0)), "outside"), "resolve = 0 is openat (leaves the dirfd)");

    printf("RESOLVE_BENEATH\n");
    CHECK(!strcmp(slurp(o2(d, "f", O_RDONLY, 0, R_BENEATH)), "inside"), "f reads 'inside'");
    CHECK(!strcmp(slurp(o2(d, "sub/g", O_RDONLY, 0, R_BENEATH)), "gee"), "sub/g");
    CHECK(!strcmp(slurp(o2(d, "sub/../f", O_RDONLY, 0, R_BENEATH)), "inside"), "sub/../f stays inside");
    CHECK(o2(d, "..", O_RDONLY, 0, R_BENEATH) == -EXDEV, ".. at the bound: EXDEV");
    CHECK(o2(d, "../o2out", O_RDONLY, 0, R_BENEATH) == -EXDEV, "../o2out: EXDEV");
    CHECK(o2(d, "sub/../..", O_RDONLY, 0, R_BENEATH) == -EXDEV, "sub/../..: EXDEV");
    CHECK(o2(d, "/tmp/o2/f", O_RDONLY, 0, R_BENEATH) == -EXDEV, "absolute path (even inside): EXDEV");
    CHECK(o2(d, "abs", O_RDONLY, 0, R_BENEATH) == -EXDEV, "absolute symlink: EXDEV");
    CHECK(o2(d, "up", O_RDONLY, 0, R_BENEATH) == -EXDEV, "relative symlink climbing out: EXDEV");
    CHECK(!strcmp(slurp(o2(d, "sub/sib", O_RDONLY, 0, R_BENEATH)), "inside"), "relative symlink staying inside");
    CHECK(o2(d, "a", O_RDONLY, 0, R_BENEATH) == -ELOOP, "symlink loop: ELOOP");
    CHECK(o2(d, "f/x", O_RDONLY, 0, R_BENEATH) == -ENOTDIR, "a file in the middle: ENOTDIR");
    CHECK(o2(d, "nope", O_RDONLY, 0, R_BENEATH) == -ENOENT, "missing: ENOENT");
    {
        // d -> sub/deep: `..` after it is sub (the physical parent), and the fd records that, so a later *at through
        // it finds sub's g, and its own `..` bound is sub.
        long sub = o2(d, "d/..", O_RDONLY | O_DIRECTORY, 0, R_BENEATH);
        CHECK(sub >= 0, "d/.. opens: %ld", sub);
        CHECK(!strcmp(slurp(o2(sub, "g", O_RDONLY, 0, R_BENEATH)), "gee"), "d/.. is sub: its g is there");
        CHECK(!strcmp(slurp(sc4(SYS_openat, sub, (long)"g", O_RDONLY, 0)), "gee"), "plain openat through that fd too");
        CHECK(o2(sub, "../f", O_RDONLY, 0, R_BENEATH) == -EXDEV, "and its own bound is sub");
        close((int)sub);
        CHECK(o2(d, "d/../..", O_RDONLY, 0, R_BENEATH) >= 0, "d/../.. is the bound itself");
        CHECK(o2(d, "d/../../..", O_RDONLY, 0, R_BENEATH) == -EXDEV, "d/../../..: EXDEV");
    }
    {
        // A mount under the bound: from /, into /mnt and back is fine; with /mnt (a mount root) as the bound, .. is out.
        long root = open("/", O_RDONLY | O_DIRECTORY);
        long mnt = open("/mnt", O_RDONLY | O_DIRECTORY);
        CHECK(o2(root, "mnt/bin/../..", O_RDONLY, 0, R_BENEATH) >= 0, "/ -> mnt/bin/../.. stays at /");
        CHECK(o2(mnt, "bin/..", O_RDONLY, 0, R_BENEATH) >= 0, "/mnt -> bin/..");
        CHECK(o2(mnt, "..", O_RDONLY, 0, R_BENEATH) == -EXDEV, "/mnt -> .. (out of the mount root): EXDEV");
        close((int)root); close((int)mnt);
    }
    {
        // AT_FDCWD: the bound is the cwd.
        char old[128];
        getcwd(old, sizeof old);
        chdir("/tmp/o2");
        CHECK(!strcmp(slurp(o2(AT_FDCWD_, "f", O_RDONLY, 0, R_BENEATH)), "inside"), "AT_FDCWD: f");
        CHECK(o2(AT_FDCWD_, "..", O_RDONLY, 0, R_BENEATH) == -EXDEV, "AT_FDCWD: ..: EXDEV");
        chdir(old);
    }
    long file = open("/tmp/o2/f", O_RDONLY);
    CHECK(o2(file, "x", O_RDONLY, 0, R_BENEATH) == -ENOTDIR, "a file as dirfd: ENOTDIR");
    close((int)file);
    CHECK(o2(999, "f", O_RDONLY, 0, R_BENEATH) == -EBADF, "a closed dirfd: EBADF");

    printf("RESOLVE_NO_SYMLINKS\n");
    CHECK(!strcmp(slurp(o2(d, "sub/g", O_RDONLY, 0, R_NO_SYMLINKS)), "gee"), "no symlink on the way: fine");
    CHECK(o2(d, "sub/sib", O_RDONLY, 0, R_NO_SYMLINKS) == -ELOOP, "final symlink: ELOOP");
    CHECK(o2(d, "d/..", O_RDONLY, 0, R_NO_SYMLINKS) == -ELOOP, "intermediate symlink: ELOOP");
    CHECK(o2(d, "abs", O_RDONLY, 0, R_NO_SYMLINKS | R_BENEATH) == -ELOOP, "with BENEATH too: ELOOP");

    printf("O_CREAT, O_EXCL, O_NOFOLLOW\n");
    long nfd = o2(d, "sub/new", O_WRONLY | O_CREAT, 0644, R_BENEATH);
    CHECK(nfd >= 0, "O_CREAT sub/new: %ld", nfd);
    close((int)nfd);
    struct stat st;
    CHECK(stat("/tmp/o2/sub/new", &st) == 0, "it exists under /tmp/o2/sub");
    CHECK(o2(d, "../o2new", O_WRONLY | O_CREAT, 0644, R_BENEATH) == -EXDEV, "O_CREAT ../o2new: EXDEV");
    CHECK(stat("/tmp/o2new", &st) != 0, "and nothing was created outside");
    CHECK(o2(d, "f", O_WRONLY | O_CREAT | O_EXCL, 0644, R_BENEATH) == -EEXIST, "O_CREAT|O_EXCL on f: EEXIST");
    CHECK(o2(d, "sub/sib", O_RDONLY | O_NOFOLLOW, 0, R_BENEATH) == -ELOOP, "O_NOFOLLOW on a symlink: ELOOP");

    close((int)d);
    printf(failures ? "openat2_test: FAIL\n" : "openat2_test: PASS\n");
    return failures != 0;
}
