// zombie_mem_test: a process gives its memory back when it exits, not when its parent gets round to wait()ing for it (Linux's
// exit_mm). Until 2026-10-01 a zombie kept its whole address space in the wait queue; a dead 19 MB Vulkan compositor held 19 MB
// until reaped, and the reap freed every page under SCHEDULER, which stops every CPU's tick and syscall entry for as long as it takes.
//
//   A. a child that touched 64 MiB and exited, not yet waited for, is a zombie (state Z in /proc/<pid>/stat) and MemFree is back
//      within a few MiB of what it was before the child existed;
//   B. the zombie's statm is all zeros (nothing mapped, nothing resident);
//   C. wait4 still returns the child's pid and its exit status;
//   D. MemFree after the wait is still the baseline (the memory was not freed twice or kept);
//   E. the same with a child that was killed by a signal (SIGKILL, which dies inside the scheduler, not in sys_exit);
//   F. the same with a child that forked a grandchild sharing the memory copy-on-write: the parent's exit frees only its own pages,
//      and the grandchild still reads its data.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/wait.h>

#define MIB (1024L * 1024L)
#define PAGE 4096L
#define BIG (64 * MIB)
// What MemFree may drift by while the test runs (other processes, kernel stacks and tables freed lazily).
#define SLACK_KB (6 * 1024L)

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

static long mem_free_kb(void) {
    if (read_file("/proc/meminfo") <= 0) return -1;
    char *p = strstr(buf, "MemFree:");
    return p ? atol(p + 8) : -1;
}

// State letter of `pid` from /proc/<pid>/stat (the field after the parenthesised name), or 0.
static char state_of(int pid) {
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/stat", pid);
    if (read_file(path) <= 0) return 0;
    char *p = strrchr(buf, ')');
    return p && p[1] == ' ' ? p[2] : 0;
}

// Waits (without wait4) until `pid` is a zombie.
static int wait_zombie(int pid) {
    for (int i = 0; i < 500; i++) {
        if (state_of(pid) == 'Z') return 1;
        usleep(10 * 1000);
    }
    return 0;
}

// Waits until MemFree is within SLACK of `base`: the freeing is done by whichever CPU drains first, shortly after the exit.
static long wait_free(long base) {
    long f = 0;
    for (int i = 0; i < 500; i++) {
        f = mem_free_kb();
        if (f >= base - SLACK_KB) return f;
        usleep(10 * 1000);
    }
    return f;
}

static void touch(char *p, long len) {
    for (long i = 0; i < len; i += PAGE) p[i] = (char)(i / PAGE + 1);
}

// The child: touch BIG bytes, say so on `ready`, then wait for `go` to close and leave through `how`.
static void child(int ready, int go, int how) {
    char *m = mmap(0, BIG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (m == MAP_FAILED) _exit(99);
    touch(m, BIG);
    write(ready, "r", 1);
    char c;
    read(go, &c, 1);
    if (how == 1) kill(getpid(), SIGKILL);
    _exit(7);
}

// One round: returns after checking A-D (or E for `how` == 1).
static void round_trip(const char *label, int how) {
    printf("%s\n", label);
    long base = mem_free_kb();
    int ready[2], go[2];
    if (pipe(ready) || pipe(go)) { check("pipe", 0); return; }
    pid_t pid = fork();
    if (pid == 0) {
        close(ready[0]); close(go[1]);
        child(ready[1], go[0], how);
    }
    close(ready[1]); close(go[0]);
    char c;
    read(ready[0], &c, 1);
    long during = mem_free_kb();
    check("the child's 64 MiB is taken while it lives", base - during >= BIG / 1024 - SLACK_KB);

    close(go[1]);  // the child leaves; nobody waits for it yet
    check("it becomes a zombie", wait_zombie(pid));
    long after = wait_free(base);
    printf("  MemFree: before %ld kB, child alive %ld kB, zombie %ld kB\n", base, during, after);
    check("its memory is free again while it is still a zombie, unwaited", after >= base - SLACK_KB);

    char path[64];
    snprintf(path, sizeof path, "/proc/%d/statm", pid);
    long size = -1, res = -1;
    if (read_file(path) > 0) sscanf(buf, "%ld %ld", &size, &res);
    check("the zombie's statm is 0 0", size == 0 && res == 0);

    int status = 0;
    pid_t w = waitpid(pid, &status, 0);
    check("wait4 returns the child", w == pid);
    if (how == 1) check("with the SIGKILL status", WIFSIGNALED(status) && WTERMSIG(status) == SIGKILL);
    else check("with its exit status", WIFEXITED(status) && WEXITSTATUS(status) == 7);
    long end = wait_free(base);
    check("MemFree after the wait is still the baseline", end >= base - SLACK_KB && end <= base + SLACK_KB);
    close(ready[0]);
}

// F: a child that forks a grandchild, then exits first.
static void shared_cow(void) {
    printf("F: parent exits while its child still uses the shared pages\n");
    long base = mem_free_kb();
    int ready[2], go[2];
    if (pipe(ready) || pipe(go)) { check("pipe", 0); return; }
    pid_t mid = fork();
    if (mid == 0) {
        close(ready[0]); close(go[1]);
        char *m = mmap(0, BIG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (m == MAP_FAILED) _exit(99);
        touch(m, BIG);
        pid_t gc = fork();
        if (gc == 0) {
            // The grandchild outlives its parent and reads the data it inherited, then writes (COW) over all of it.
            char c;
            write(ready[1], "g", 1);
            read(go[0], &c, 1);
            for (long i = 0; i < BIG; i += PAGE)
                if (m[i] != (char)(i / PAGE + 1)) _exit(3);
            for (long i = 0; i < BIG; i += PAGE) m[i] = 1;
            _exit(0);
        }
        _exit(0);  // exits at once, the grandchild holds the frames
    }
    close(ready[1]); close(go[0]);
    char c;
    read(ready[0], &c, 1);
    int status;
    check("the middle process is reaped", waitpid(mid, &status, 0) == mid && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    long held = mem_free_kb();
    check("the grandchild still holds the 64 MiB", base - held >= BIG / 1024 - SLACK_KB);
    close(go[1]);  // the grandchild checks its data and exits
    // It is an orphan now: PID 1 reaps it. Wait for its memory to come back.
    long end = wait_free(base);
    check("everything is free once the grandchild is done", end >= base - SLACK_KB);
    close(ready[0]);
}

int main(void) {
    printf("zombie_mem_test\n");
    round_trip("A-D: exit(7)", 0);
    round_trip("E: SIGKILL", 1);
    shared_cow();
    round_trip("repeat (a leak would add up)", 0);
    printf("zombie_mem_test: %s (%d failure%s)\n", fails ? "FAIL" : "PASS", fails, fails == 1 ? "" : "s");
    return fails ? 1 : 0;
}
