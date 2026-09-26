// fstime_test: file timestamps and the libc pieces that read them.
// Until 2026-09-26 every stat() reported 1970 (no filesystem filled in a
// time), utimensat did not exist (touch: ENOSYS), ctime() was one
// character short (mlibc's asctime_r lost a ':'), sscanf ignored integer
// field widths (touch -t: "invalid date") and there was no /etc/passwd.
//
//   A. ramfs (/tmp): creation stamps all three times; a write moves mtime
//      and ctime, not atime; chmod moves only ctime;
//   B. utimensat: exact times, UTIME_OMIT keeps one, UTIME_NOW is now, a
//      NULL times[] is now; futimens through an fd; AT_SYMLINK_NOFOLLOW on
//      a symlink leaves its target alone; EROFS where nothing keeps times;
//      EINVAL for a bad tv_nsec;
//   C. ext2 (/mnt), if it is writable: a new file is stamped now, and
//      utimensat's times survive a fresh stat;
//   D. /proc and /dev report the boot time, not the epoch;
//   E. libc: ctime() is 25 characters, sscanf honours %4u%2u widths
//      (and for %x, %o, %d with a sign), getpwuid(0) is root.
#include <errno.h>
#include <fcntl.h>
#include <pwd.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/stat.h>

static int fails;

static void check(const char *what, int ok) {
    printf("  %s -> %s\n", what, ok ? "PASS" : "FAIL");
    if (!ok) fails++;
}

static struct stat st_of(const char *p) {
    struct stat st;
    memset(&st, 0, sizeof st);
    if (lstat(p, &st) != 0) printf("    lstat(%s): errno %d\n", p, errno);
    return st;
}

static void nap_s(int s) {
    struct timespec ts = { s, 0 };
    nanosleep(&ts, NULL);
}

static void write_file(const char *p, const char *s) {
    int fd = open(p, O_WRONLY | O_CREAT | O_APPEND, 0644);
    if (fd >= 0) {
        write(fd, s, strlen(s));
        close(fd);
    }
}

static void case_ramfs(void) {
    printf("A. ramfs timestamps\n");
    unlink("/tmp/fst_a");
    time_t before = time(NULL);
    write_file("/tmp/fst_a", "x");
    struct stat born = st_of("/tmp/fst_a");
    printf("    born a=%lld m=%lld c=%lld now=%lld\n", (long long)born.st_atime,
           (long long)born.st_mtime, (long long)born.st_ctime, (long long)before);
    check("created now", born.st_mtime >= before && born.st_mtime <= time(NULL));
    check("atime == mtime == ctime at birth",
          born.st_atime == born.st_mtime && born.st_ctime == born.st_mtime);

    nap_s(2);
    write_file("/tmp/fst_a", "y");
    struct stat w = st_of("/tmp/fst_a");
    check("write moved mtime", w.st_mtime > born.st_mtime);
    check("write moved ctime with it", w.st_ctime == w.st_mtime);
    check("write left atime", w.st_atime == born.st_atime);

    nap_s(2);
    chmod("/tmp/fst_a", 0600);
    struct stat c = st_of("/tmp/fst_a");
    check("chmod moved ctime only", c.st_ctime > w.st_ctime && c.st_mtime == w.st_mtime);
}

static void case_utimensat(void) {
    printf("B. utimensat\n");
    const char *p = "/tmp/fst_a";
    struct timespec t[2] = { { 1000000000, 0 }, { 1200000000, 0 } };
    check("set both", utimensat(AT_FDCWD, p, t, 0) == 0);
    struct stat s = st_of(p);
    check("exact atime and mtime", s.st_atime == 1000000000 && s.st_mtime == 1200000000);
    check("ctime moved to now", s.st_ctime >= time(NULL) - 2);

    struct timespec omit[2] = { { 0, UTIME_OMIT }, { 1300000000, 0 } };
    utimensat(AT_FDCWD, p, omit, 0);
    s = st_of(p);
    check("UTIME_OMIT keeps atime", s.st_atime == 1000000000 && s.st_mtime == 1300000000);

    struct timespec now[2] = { { 0, UTIME_NOW }, { 0, UTIME_OMIT } };
    utimensat(AT_FDCWD, p, now, 0);
    s = st_of(p);
    check("UTIME_NOW sets now", s.st_atime >= time(NULL) - 2 && s.st_mtime == 1300000000);

    check("NULL times = now", utimensat(AT_FDCWD, p, NULL, 0) == 0 && st_of(p).st_mtime >= time(NULL) - 2);

    int fd = open(p, O_RDONLY);
    struct timespec f[2] = { { 1400000000, 0 }, { 1500000000, 0 } };
    check("futimens", fd >= 0 && futimens(fd, f) == 0);
    close(fd);
    s = st_of(p);
    check("futimens landed", s.st_atime == 1400000000 && s.st_mtime == 1500000000);

    unlink("/tmp/fst_l");
    symlink("/tmp/fst_a", "/tmp/fst_l");
    struct timespec l[2] = { { 1600000000, 0 }, { 1600000000, 0 } };
    check("AT_SYMLINK_NOFOLLOW", utimensat(AT_FDCWD, "/tmp/fst_l", l, AT_SYMLINK_NOFOLLOW) == 0);
    check("...set the link", st_of("/tmp/fst_l").st_mtime == 1600000000);
    check("...not its target", st_of(p).st_mtime == 1500000000);

    errno = 0;
    check("EROFS on /proc", utimensat(AT_FDCWD, "/proc/meminfo", NULL, 0) == -1 && errno == EROFS);
    struct timespec bad[2] = { { 0, 1000000000 }, { 0, 0 } };
    errno = 0;
    check("EINVAL for tv_nsec out of range", utimensat(AT_FDCWD, p, bad, 0) == -1 && errno == EINVAL);
    errno = 0;
    check("ENOENT for a missing path", utimensat(AT_FDCWD, "/tmp/nope/x", NULL, 0) == -1 && errno == ENOENT);
}

static void case_ext2(void) {
    printf("C. ext2 timestamps\n");
    unlink("/mnt/fst_e");
    int fd = open("/mnt/fst_e", O_WRONLY | O_CREAT, 0644);
    if (fd < 0) {
        printf("  /mnt is not writable (errno %d): skipped\n", errno);
        return;
    }
    write(fd, "e", 1);
    close(fd);
    struct stat s = st_of("/mnt/fst_e");
    check("new file stamped now", s.st_mtime >= time(NULL) - 2 && s.st_atime == s.st_mtime);
    struct timespec t[2] = { { 1111111111, 0 }, { 1222222222, 0 } };
    check("utimensat", utimensat(AT_FDCWD, "/mnt/fst_e", t, 0) == 0);
    s = st_of("/mnt/fst_e");
    check("times read back from the inode", s.st_atime == 1111111111 && s.st_mtime == 1222222222);
    check("a pre-existing file has a real date", st_of("/mnt/bin").st_mtime > 1600000000);
    unlink("/mnt/fst_e");
}

static void case_synthetic(void) {
    printf("D. synthetic filesystems\n");
    struct timespec up;
    clock_gettime(CLOCK_MONOTONIC, &up);
    long long boot = (long long)time(NULL) - up.tv_sec;
    long long p = st_of("/proc/meminfo").st_mtime, d = st_of("/dev/null").st_mtime;
    printf("    boot~%lld proc=%lld dev=%lld\n", boot, p, d);
    check("/proc at boot time", p >= boot - 2 && p <= boot + 2);
    check("/dev at boot time", d >= boot - 2 && d <= boot + 2);
}

static void case_libc(void) {
    printf("E. libc\n");
    time_t t = 0;
    char *s = ctime(&t);
    printf("    ctime(0) = %s", s);
    check("ctime is 25 characters with its newline", strlen(s) == 25);
    check("ctime of the epoch", strcmp(s, "Thu Jan  1 00:00:00 1970\n") == 0);

    unsigned y = 0, mo = 0, d = 0, h = 0, mi = 0;
    int n = sscanf("201501020304", "%4u%2u%2u%2u%2u", &y, &mo, &d, &h, &mi);
    printf("    sscanf -> %d: %u %u %u %u %u\n", n, y, mo, d, h, mi);
    check("%4u%2u... splits by width", n == 5 && y == 2015 && mo == 1 && d == 2 && h == 3 && mi == 4);
    int a = 0, b = 0;
    check("%3d counts the sign", sscanf("-1234", "%3d%d", &a, &b) == 2 && a == -12 && b == 34);
    unsigned x = 0, o = 0;
    check("%2x and %3o", sscanf("ff177", "%2x%3o", &x, &o) == 2 && x == 255 && o == 127);
    check("%x of a lone 0", sscanf("0", "%x", &x) == 1 && x == 0);
    check("%x with its 0x prefix", sscanf("0x1F", "%x", &x) == 1 && x == 31);

    struct passwd *pw = getpwuid(0);
    check("getpwuid(0) is root", pw && strcmp(pw->pw_name, "root") == 0);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    case_ramfs();
    case_utimensat();
    case_ext2();
    case_synthetic();
    case_libc();
    unlink("/tmp/fst_a");
    unlink("/tmp/fst_l");
    printf("fstime_test: %s (%d failure%s)\n", fails ? "FAIL" : "PASS", fails, fails == 1 ? "" : "s");
    return fails ? 1 : 0;
}
