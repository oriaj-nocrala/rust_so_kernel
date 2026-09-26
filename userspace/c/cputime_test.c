// cputime_test: CPU time accounting (sched::cputime) and the POSIX/Linux
// interfaces over it. Until 2026-09-26 none of it existed: /proc/<pid>/stat
// reported 0 for every time field, there was no /proc/stat, times() and
// getrusage() were ENOSYS, clock() failed, and sysconf() said the clock
// ticked 1000000 times a second on a machine with one CPU.
//
//   A. sysconf(_SC_CLK_TCK) is 100; _SC_NPROCESSORS_ONLN, sched_getaffinity,
//      the cpuN lines of /proc/stat and the processors of /proc/cpuinfo
//      all agree;
//   B. /proc/stat's aggregate line is the column sum of its cpuN lines;
//   C. spinning ~300 ms of CPU shows up in times(), getrusage(SELF),
//      clock(), CLOCK_PROCESS_CPUTIME_ID and /proc/self/stat, as user time;
//   D. sleeping 300 ms does not;
//   E. a child's time reaches cutime only once it is waited for, and a
//      grandchild's reaches it through the child;
//   F. a thread's time counts for the process (CLOCK_PROCESS_CPUTIME_ID,
//      times() after it exits) but not for the main thread's
//      CLOCK_THREAD_CPUTIME_ID;
//   G. (2+ CPUs) children spinning at once are charged to different CPUs;
//   H. /proc/uptime, btime, sysinfo() and /proc/loadavg are consistent
//      with the clocks;
//   I. error cases: getrusage(bad who), sched_getaffinity(short buffer),
//      clock_getres of a CPU-time clock.
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/resource.h>
#include <sys/sysinfo.h>
#include <sys/times.h>
#include <sys/wait.h>

#define MAX_CPUS 64

static int fails;

static void check(const char *what, int ok) {
    printf("  %s -> %s\n", what, ok ? "PASS" : "FAIL");
    if (!ok) fails++;
}

static long long ns_of(clockid_t c) {
    struct timespec ts;
    if (clock_gettime(c, &ts) != 0) return -1;
    return (long long)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

// Burn `ms` of this thread's CPU time (not wall time: under load the two
// differ, and it is CPU time that is being measured).
static volatile unsigned long sink;
static void spin_ms(long ms) {
    long long end = ns_of(CLOCK_THREAD_CPUTIME_ID) + ms * 1000000LL;
    // Check the clock every ~1 ms of work, not every few microseconds:
    // this is meant to be user time, and a loop that is mostly
    // clock_gettime() calls is mostly system time (it was, at first).
    while (ns_of(CLOCK_THREAD_CPUTIME_ID) < end)
        for (int i = 0; i < 1000000; i++) sink += i;
}

static void nap_ms(long ms) {
    struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L };
    nanosleep(&ts, NULL);
}

static int read_file(const char *path, char *buf, size_t len) {
    FILE *f = fopen(path, "r");
    if (!f) return -1;
    size_t n = fread(buf, 1, len - 1, f);
    buf[n] = 0;
    fclose(f);
    return (int)n;
}

struct cpu { int id; unsigned long long f[10]; };

// Parse /proc/stat: the aggregate into *all, cpuN lines into cpus[]. Returns
// the number of cpuN lines, -1 on a malformed file.
static int read_stat(struct cpu *all, struct cpu *cpus) {
    static char buf[8192];
    if (read_file("/proc/stat", buf, sizeof buf) <= 0) return -1;
    int n = 0;
    for (char *line = strtok(buf, "\n"); line; line = strtok(NULL, "\n")) {
        if (strncmp(line, "cpu", 3) != 0) continue;
        struct cpu c = { -1, {0} };
        char *p = line + 3;
        if (*p != ' ') c.id = (int)strtol(p, &p, 10);
        for (int i = 0; i < 10; i++) c.f[i] = strtoull(p, &p, 10);
        if (c.id < 0) *all = c;
        else if (n < MAX_CPUS) cpus[n++] = c;
    }
    return n;
}

// Field `k` (1-based, as in proc(5)) of /proc/self/stat.
static long long self_stat(int k) {
    char buf[1024];
    if (read_file("/proc/self/stat", buf, sizeof buf) <= 0) return -1;
    char *p = strrchr(buf, ')');
    if (!p) return -1;
    p += 2; // field 3 starts here
    for (int i = 3; i < k; i++) {
        p = strchr(p, ' ');
        if (!p) return -1;
        p++;
    }
    return strtoll(p, NULL, 10);
}

static int affinity_count(void) {
    cpu_set_t set;
    if (sched_getaffinity(0, sizeof set, &set) != 0) return -1;
    int n = 0;
    for (size_t i = 0; i < sizeof set; i++) n += __builtin_popcount(((unsigned char *)&set)[i]);
    return n;
}

static void case_counts(void) {
    printf("A. clock tick and CPU count\n");
    check("sysconf(_SC_CLK_TCK) == 100", sysconf(_SC_CLK_TCK) == 100);
    long onln = sysconf(_SC_NPROCESSORS_ONLN);
    int aff = affinity_count();
    struct cpu all, cpus[MAX_CPUS];
    int lines = read_stat(&all, cpus);
    char buf[16384];
    int procs = 0;
    if (read_file("/proc/cpuinfo", buf, sizeof buf) > 0)
        for (char *p = buf; (p = strstr(p, "processor\t:")); p++) procs++;
    printf("    onln=%ld affinity=%d stat_lines=%d cpuinfo=%d\n", onln, aff, lines, procs);
    check("_SC_NPROCESSORS_ONLN >= 1", onln >= 1);
    check("sched_getaffinity agrees", aff == onln);
    check("/proc/stat has one cpuN line per CPU", lines == onln);
    check("/proc/cpuinfo has one block per CPU", procs == onln);
    check("/proc/cpuinfo names the model", strstr(buf, "model name\t: ") != NULL);
}

static void case_stat_sum(void) {
    printf("B. /proc/stat aggregate is the sum of the cpuN lines\n");
    struct cpu all, cpus[MAX_CPUS];
    int n = read_stat(&all, cpus);
    int ok = n > 0;
    for (int col = 0; col < 10 && ok; col++) {
        unsigned long long sum = 0;
        for (int i = 0; i < n; i++) sum += cpus[i].f[col];
        ok = sum == all.f[col];
    }
    check("every column adds up", ok);
    check("idle ticks are counted", all.f[3] > 0);
}

static void case_spin(void) {
    printf("C. 300 ms of CPU is accounted as user time\n");
    struct tms t0, t1;
    struct rusage r0, r1;
    times(&t0);
    getrusage(RUSAGE_SELF, &r0);
    long long p0 = ns_of(CLOCK_PROCESS_CPUTIME_ID);
    clock_t c0 = clock();
    long long s0 = self_stat(14);

    spin_ms(300);

    times(&t1);
    getrusage(RUSAGE_SELF, &r1);
    long long p1 = ns_of(CLOCK_PROCESS_CPUTIME_ID);
    clock_t c1 = clock();
    long long s1 = self_stat(14);
    long du = (long)(t1.tms_utime - t0.tms_utime), ds = (long)(t1.tms_stime - t0.tms_stime);
    long long ru = (r1.ru_utime.tv_sec - r0.ru_utime.tv_sec) * 1000000LL + (r1.ru_utime.tv_usec - r0.ru_utime.tv_usec);
    printf("    times: +%ld user +%ld sys ticks; rusage +%lld us; cputime +%lld ms; clock +%ld us; stat +%lld\n",
           du, ds, ru, (p1 - p0) / 1000000, (long)(c1 - c0), s1 - s0);
    check("CLOCK_PROCESS_CPUTIME_ID advanced >= 300 ms", p1 - p0 >= 300000000LL);
    check("clock() advanced >= 300000 us", c1 - c0 >= 300000);
    // Ticks are samples: allow a few to have landed elsewhere.
    check("times() user >= 25 ticks", du >= 25);
    check("user dominates system", du > ds);
    check("getrusage(SELF) user >= 250 ms", ru >= 250000);
    check("/proc/self/stat utime moved like times()", s1 - s0 >= du - 2 && s1 - s0 <= du + 2);
}

static void case_sleep(void) {
    printf("D. 300 ms of sleep is not CPU time\n");
    struct tms t0, t1;
    long long p0 = ns_of(CLOCK_PROCESS_CPUTIME_ID);
    times(&t0);
    nap_ms(300);
    times(&t1);
    long long p1 = ns_of(CLOCK_PROCESS_CPUTIME_ID);
    printf("    cputime +%lld us, user +%ld sys +%ld ticks\n", (p1 - p0) / 1000,
           (long)(t1.tms_utime - t0.tms_utime), (long)(t1.tms_stime - t0.tms_stime));
    check("CLOCK_PROCESS_CPUTIME_ID advanced < 30 ms", p1 - p0 < 30000000LL);
    check("times() moved < 3 ticks", (t1.tms_utime - t0.tms_utime) + (t1.tms_stime - t0.tms_stime) < 3);
}

static void case_children(void) {
    printf("E. children's time reaches cutime when waited for\n");
    struct tms t0, t1, t2;
    times(&t0);
    int fd[2];
    pipe(fd);
    pid_t pid = fork();
    if (pid == 0) {
        close(fd[0]);
        pid_t g = fork();
        if (g == 0) { spin_ms(200); _exit(0); }
        spin_ms(200);
        waitpid(g, NULL, 0);
        write(fd[1], "x", 1);
        _exit(0);
    }
    close(fd[1]);
    char c;
    read(fd[0], &c, 1); // the child has spun and reaped its own child
    nap_ms(50);         // and has exited (or nearly): still not waited for
    times(&t1);
    int st;
    waitpid(pid, &st, 0);
    times(&t2);
    close(fd[0]);
    struct rusage rc;
    getrusage(RUSAGE_CHILDREN, &rc);
    long before = (long)(t1.tms_cutime - t0.tms_cutime), after = (long)(t2.tms_cutime - t0.tms_cutime);
    printf("    cutime before wait +%ld, after +%ld ticks; rusage children %ld.%06ld s\n",
           before, after, (long)rc.ru_utime.tv_sec, (long)rc.ru_utime.tv_usec);
    check("nothing before waitpid", before == 0);
    check("child + grandchild after waitpid (>= 35 ticks)", after >= 35);
    check("getrusage(CHILDREN) >= 350 ms", rc.ru_utime.tv_sec * 1000000L + rc.ru_utime.tv_usec >= 350000);
}

static void *thread_spin(void *arg) {
    (void)arg;
    spin_ms(200);
    return NULL;
}

static void case_thread(void) {
    printf("F. a thread's time is the process's, not the main thread's\n");
    struct tms t0, t1;
    times(&t0);
    long long p0 = ns_of(CLOCK_PROCESS_CPUTIME_ID), m0 = ns_of(CLOCK_THREAD_CPUTIME_ID);
    pthread_t th;
    pthread_create(&th, NULL, thread_spin, NULL);
    pthread_join(th, NULL);
    long long p1 = ns_of(CLOCK_PROCESS_CPUTIME_ID), m1 = ns_of(CLOCK_THREAD_CPUTIME_ID);
    times(&t1);
    printf("    process +%lld ms, main thread +%lld ms, times user +%ld ticks\n",
           (p1 - p0) / 1000000, (m1 - m0) / 1000000, (long)(t1.tms_utime - t0.tms_utime));
    check("process CPU clock includes the thread (>= 200 ms)", p1 - p0 >= 200000000LL);
    check("main thread's CPU clock does not (< 50 ms)", m1 - m0 < 50000000LL);
    check("times() keeps the exited thread's time (>= 15 ticks)", t1.tms_utime - t0.tms_utime >= 15);
}

static void case_smp(void) {
    long n = sysconf(_SC_NPROCESSORS_ONLN);
    printf("G. parallel spinners are charged to several CPUs (%ld online)\n", n);
    if (n < 2) {
        printf("  one CPU: skipped\n");
        return;
    }
    int kids = n < 4 ? (int)n : 4;
    struct cpu a0, c0[MAX_CPUS], a1, c1[MAX_CPUS];
    int lines = read_stat(&a0, c0);
    pid_t pids[4];
    for (int i = 0; i < kids; i++) {
        pids[i] = fork();
        if (pids[i] == 0) { spin_ms(500); _exit(0); }
    }
    for (int i = 0; i < kids; i++) waitpid(pids[i], NULL, 0);
    read_stat(&a1, c1);
    int busy = 0;
    for (int i = 0; i < lines; i++) {
        unsigned long long du = c1[i].f[0] - c0[i].f[0];
        printf("    cpu%d +%llu user\n", c1[i].id, du);
        if (du >= 20) busy++;
    }
    check("at least two CPUs gained >= 20 user ticks", busy >= 2);
    check("aggregate user grew by >= kids*40 ticks", a1.f[0] - a0.f[0] >= (unsigned long long)kids * 40);
}

static void case_clocks(void) {
    printf("H. uptime, btime, sysinfo and loadavg agree with the clocks\n");
    char buf[4096];
    // Centiseconds, parsed as "S.CC" by hand: no floating point needed.
    long up = -1, idle = -1;
    if (read_file("/proc/uptime", buf, sizeof buf) > 0) {
        long s1, c1, s2, c2;
        if (sscanf(buf, "%ld.%ld %ld.%ld", &s1, &c1, &s2, &c2) == 4) {
            up = s1 * 100 + c1;
            idle = s2 * 100 + c2;
        }
    }
    long mono = (long)(ns_of(CLOCK_MONOTONIC) / 10000000LL);
    printf("    /proc/uptime %ld.%02ld idle %ld.%02ld, monotonic %ld.%02ld\n",
           up / 100, up % 100, idle / 100, idle % 100, mono / 100, mono % 100);
    check("/proc/uptime matches CLOCK_MONOTONIC (0.1 s)", up > 0 && up - mono <= 10 && mono - up <= 10);
    check("idle is non-negative", idle >= 0);

    long long btime = -1;
    if (read_file("/proc/stat", buf, sizeof buf) > 0) {
        char *p = strstr(buf, "\nbtime ");
        if (p) btime = strtoll(p + 7, NULL, 10);
    }
    long long now = time(NULL);
    printf("    btime %lld + uptime %ld vs time() %lld\n", btime, up / 100, now);
    check("btime + uptime == time() (2 s)", btime >= 0 && btime + up / 100 - now <= 2 && now - btime - up / 100 <= 2);

    struct sysinfo si;
    check("sysinfo() succeeds", sysinfo(&si) == 0);
    printf("    sysinfo: uptime %ld procs %d totalram %lu freeram %lu unit %u\n",
           si.uptime, si.procs, si.totalram, si.freeram, si.mem_unit);
    check("sysinfo uptime matches (1 s)", si.uptime - up / 100 <= 1 && up / 100 - si.uptime <= 1);
    check("sysinfo counts processes", si.procs >= 1);
    check("sysinfo freeram <= totalram", si.totalram > 0 && si.freeram <= si.totalram);

    int a[6], run, total, last;
    int got = read_file("/proc/loadavg", buf, sizeof buf) > 0
        ? sscanf(buf, "%d.%d %d.%d %d.%d %d/%d %d", &a[0], &a[1], &a[2], &a[3], &a[4], &a[5], &run, &total, &last) : 0;
    printf("    /proc/loadavg: %s", buf);
    check("/proc/loadavg has Linux's six fields", got == 9);
    check("at least this process is runnable", got == 9 && run >= 1 && total >= run);
}

static void case_errors(void) {
    printf("I. error cases\n");
    struct rusage r;
    errno = 0;
    check("getrusage(42) is EINVAL", getrusage(42, &r) == -1 && errno == EINVAL);
    unsigned char tiny[2];
    errno = 0;
    check("sched_getaffinity(2 bytes) is EINVAL",
          sched_getaffinity(0, sizeof tiny, (cpu_set_t *)tiny) == -1 && errno == EINVAL);
    struct timespec res;
    check("clock_getres(CLOCK_PROCESS_CPUTIME_ID) is 1 ns",
          clock_getres(CLOCK_PROCESS_CPUTIME_ID, &res) == 0 && res.tv_sec == 0 && res.tv_nsec == 1);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    case_counts();
    case_stat_sum();
    case_spin();
    case_sleep();
    case_children();
    case_thread();
    case_smp();
    case_clocks();
    case_errors();
    printf("cputime_test: %s (%d failure%s)\n", fails ? "FAIL" : "PASS", fails, fails == 1 ? "" : "s");
    return fails ? 1 : 0;
}
