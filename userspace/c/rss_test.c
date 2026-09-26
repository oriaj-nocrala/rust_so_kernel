// rss_test: resident set size in /proc/<pid>/stat (field 24) and
// /proc/<pid>/statm. Until 2026-09-26 rss was always 0 and statm did not
// exist; now the kernel walks each VMA's page table under the address-space
// lock (hal::paging::count_resident).
//
// An anonymous mmap below 2 MiB is 4 KiB pages; one of 2 MiB or more is a
// Huge2M VMA, backed by whole 2 MiB pages (sys_mmap_anon) — touching one
// byte makes 512 pages resident, and there is no huge zero page.
//
//   A. statm has seven fields; stat's vsize and rss agree with its size and
//      resident; lib and dt are 0;
//   B. a fresh 1 MiB mapping adds 256 pages of size and ~none of resident;
//      writing every page adds 256 resident;
//   C. reading every page of a fresh 1 MiB mapping adds ~none: the shared
//      zero frame is not resident;
//   D. munmap takes the written pages back out of resident;
//   E. huge mappings: size at mmap, 512 resident per 2 MiB touched, read or
//      written, and munmap gives them back;
//   F. fork: the child starts with its parent's resident set and sees its
//      parent's data, in 4 KiB (COW) and huge (copied) mappings alike —
//      until 2026-09-26 fork skipped every huge page and the child read
//      zeros there;
//   G. a written MAP_SHARED memfd mapping counts as resident and shared;
//   H. another process's statm (PID 1's) is readable, and /proc/self lists
//      statm.
//
// Reads go through open/read into static buffers, not stdio or malloc, so
// taking a measurement does not itself fault in pages between two readings.
#define _GNU_SOURCE
#include <dirent.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/wait.h>

#define PAGE 4096L
#define MIB (1024L * 1024L)
// Pages a measurement may move by on its own (stack, a libc buffer).
#define SLACK 8

static int fails;

static void check(const char *what, int ok) {
    printf("  %s -> %s\n", what, ok ? "PASS" : "FAIL");
    if (!ok) fails++;
}

static char buf[1024];

static int read_file(const char *path) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    int n = read(fd, buf, sizeof buf - 1);
    close(fd);
    if (n < 0) return -1;
    buf[n] = 0;
    return n;
}

struct statm { long size, resident, shared, text, lib, data, dt; int fields; };

static struct statm statm_of(const char *path) {
    struct statm m = {0};
    if (read_file(path) > 0) {
        m.fields = sscanf(buf, "%ld %ld %ld %ld %ld %ld %ld",
                          &m.size, &m.resident, &m.shared, &m.text, &m.lib, &m.data, &m.dt);
    }
    return m;
}

static long resident(void) { return statm_of("/proc/self/statm").resident; }

// Fields 23 (vsize, bytes) and 24 (rss, pages) of /proc/self/stat.
static int stat_mem(unsigned long *vsize, long *rss) {
    if (read_file("/proc/self/stat") <= 0) return 0;
    char *p = strrchr(buf, ')');
    if (!p) return 0;
    p += 2; // field 3
    for (int field = 3; field < 23; field++) {
        p = strchr(p, ' ');
        if (!p) return 0;
        p++;
    }
    return sscanf(p, "%lu %ld", vsize, rss) == 2;
}

static long labs_(long x) { return x < 0 ? -x : x; }

int main(void) {
    printf("A. statm and stat agree\n");
    struct statm m = statm_of("/proc/self/statm");
    unsigned long vsize = 0;
    long rss = -1;
    int got = stat_mem(&vsize, &rss);
    printf("  statm: %ld %ld %ld %ld %ld %ld %ld; stat vsize=%lu rss=%ld\n",
           m.size, m.resident, m.shared, m.text, m.lib, m.data, m.dt, vsize, rss);
    check("statm has 7 fields", m.fields == 7);
    check("resident > 0", m.resident > 0);
    check("resident <= size", m.resident <= m.size);
    check("text > 0 and text + data <= size", m.text > 0 && m.text + m.data <= m.size);
    check("lib and dt are 0", m.lib == 0 && m.dt == 0);
    check("stat parsed", got);
    check("stat vsize == statm size * 4096", vsize == (unsigned long)m.size * PAGE);
    check("stat rss ~ statm resident", labs_(rss - m.resident) <= SLACK);

    printf("B. 4 KiB anonymous mapping: size at mmap, resident when written\n");
    long before_size = statm_of("/proc/self/statm").size;
    long before = resident();
    char *a = mmap(NULL, MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check("mmap 1 MiB", a != MAP_FAILED);
    struct statm after_map = statm_of("/proc/self/statm");
    printf("  size %ld -> %ld, resident %ld -> %ld\n", before_size, after_map.size,
           before, after_map.resident);
    check("size grew by 256 pages", after_map.size - before_size == 256);
    check("resident did not", after_map.resident - before <= SLACK);
    for (long i = 0; i < MIB; i += PAGE) a[i] = 1;
    long written = resident();
    printf("  after writing every page: resident %ld (+%ld)\n", written, written - before);
    check("resident grew by ~256", written - before >= 256 && written - before <= 256 + SLACK);

    printf("C. reading fresh pages maps the zero frame, which is not resident\n");
    volatile char *z = mmap(NULL, MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check("mmap 1 MiB", z != MAP_FAILED);
    long r0 = resident();
    long sum = 0;
    for (long i = 0; i < MIB; i += PAGE) sum += z[i];
    long r1 = resident();
    printf("  resident %ld -> %ld after reading 256 pages (sum %ld)\n", r0, r1, sum);
    check("reads are all zero", sum == 0);
    check("resident grew by ~none", r1 - r0 <= SLACK);
    z[0] = 1; // the zero frame's first write gets a private page
    long r1w = resident();
    check("one write makes one page resident", r1w - r1 >= 1 && r1w - r1 <= 1 + SLACK);

    printf("D. munmap releases the written pages\n");
    long r2 = resident();
    check("munmap", munmap(a, MIB) == 0);
    long r3 = resident();
    printf("  resident %ld -> %ld\n", r2, r3);
    check("resident fell by ~256", r2 - r3 >= 256 - SLACK && r2 - r3 <= 256 + SLACK);
    munmap((void *)z, MIB);

    printf("E. huge mappings are resident 2 MiB at a time\n");
    long hs = statm_of("/proc/self/statm").size;
    long h0 = resident();
    volatile char *h = mmap(NULL, 4 * MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check("mmap 4 MiB", h != MAP_FAILED);
    struct statm hm = statm_of("/proc/self/statm");
    check("size grew by 1024 pages", hm.size - hs == 1024);
    check("resident did not", hm.resident - h0 <= SLACK);
    (void)h[0];              // a read: a whole huge page, no zero frame
    long h1 = resident();
    h[3 * MIB] = 1;          // a write in the second one
    long h2 = resident();
    printf("  resident %ld -> %ld (read) -> %ld (write)\n", h0, h1, h2);
    check("a read makes 512 resident", h1 - h0 >= 512 && h1 - h0 <= 512 + SLACK);
    check("a write in the next makes 512 more", h2 - h1 >= 512 && h2 - h1 <= 512 + SLACK);
    check("munmap", munmap((void *)h, 4 * MIB) == 0);
    long h3 = resident();
    check("munmap gives 1024 back", h2 - h3 >= 1024 - SLACK && h2 - h3 <= 1024 + SLACK);

    printf("F. fork: the child has its parent's pages and data\n");
    char *small = mmap(NULL, 64 * 1024, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *b = mmap(NULL, 4 * MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    for (long i = 0; i < 64 * 1024; i += PAGE) small[i] = 7;
    for (long i = 0; i < 4 * MIB; i += PAGE) b[i] = (char)(i / PAGE);
    long parent = resident();
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        long child = resident();
        int bad_small = 0, bad_huge = 0;
        for (long i = 0; i < 64 * 1024; i += PAGE) bad_small += small[i] != 7;
        for (long i = 0; i < 4 * MIB; i += PAGE) bad_huge += b[i] != (char)(i / PAGE);
        // Writes in the child must not reach the parent (checked there).
        small[0] = 9;
        b[0] = 9;
        printf("  parent %ld, child %ld; wrong pages: %d small, %d huge\n",
               parent, child, bad_small, bad_huge);
        fflush(stdout);
        _exit((labs_(child - parent) <= SLACK ? 0 : 1) | (bad_small ? 2 : 0) | (bad_huge ? 4 : 0));
    }
    int st = 0;
    waitpid(pid, &st, 0);
    int code = WIFEXITED(st) ? WEXITSTATUS(st) : 0xff;
    check("child exited", WIFEXITED(st));
    check("child resident ~ parent's", !(code & 1));
    check("child sees the 4 KiB (COW) pages' data", !(code & 2));
    check("child sees the huge pages' data", !(code & 4));
    check("child's writes stayed in the child", small[0] == 7 && b[0] == 0);
    munmap(small, 64 * 1024);
    munmap(b, 4 * MIB);

    printf("G. a shared memfd mapping is resident and shared\n");
    int fd = memfd_create("rss_test", 0);
    check("memfd_create", fd >= 0);
    check("ftruncate 1 MiB", ftruncate(fd, MIB) == 0);
    long s0 = statm_of("/proc/self/statm").shared;
    char *s = mmap(NULL, MIB, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    check("mmap MAP_SHARED", s != MAP_FAILED);
    long r4 = resident();
    for (long i = 0; i < MIB; i += PAGE) s[i] = 3;
    struct statm ms = statm_of("/proc/self/statm");
    printf("  shared %ld -> %ld, resident %ld -> %ld\n", s0, ms.shared, r4, ms.resident);
    check("shared grew by 256", ms.shared - s0 == 256);
    check("resident grew by ~256", ms.resident - r4 >= 256 && ms.resident - r4 <= 256 + SLACK);
    munmap(s, MIB);
    close(fd);
    check("shared back to where it was", statm_of("/proc/self/statm").shared == s0);

    printf("H. other processes, and the directory listing\n");
    struct statm init = statm_of("/proc/1/statm");
    printf("  /proc/1/statm: %ld %ld %ld %ld %ld %ld %ld\n",
           init.size, init.resident, init.shared, init.text, init.lib, init.data, init.dt);
    check("/proc/1/statm has 7 fields", init.fields == 7);
    check("/proc/1/statm resident <= size", init.resident <= init.size);
    int listed = 0;
    DIR *d = opendir("/proc/self");
    if (d) {
        struct dirent *e;
        while ((e = readdir(d))) {
            if (strcmp(e->d_name, "statm") == 0) listed = 1;
        }
        closedir(d);
    }
    check("/proc/self lists statm", listed);

    printf("rss_test: %s (%d failure%s)\n", fails ? "FAIL" : "PASS", fails, fails == 1 ? "" : "s");
    return fails ? 1 : 0;
}
