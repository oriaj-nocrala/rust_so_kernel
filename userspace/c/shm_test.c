// shm_test: shared memory — memfd_create, ftruncate, mmap(MAP_SHARED).
// Phase 1 of docs/gui/gui-plan.md; each case below is one of the plan's.
//
//   1. memfd + ftruncate + mmap: write through the mapping, read it back
//      through the fd and the other way round; fstat reports the size.
//   2. fork: the child's writes reach the parent and the parent's reach
//      the child — the pages are shared, not COW.
//   3. SCM_RIGHTS: a process that did not inherit the memfd receives it,
//      maps it, and its writes are seen by the sender.
//   4. The mapping outlives the fd (close first), and the fd outlives the
//      mapping (munmap first).
//   5. MAP_SHARED|MAP_ANONYMOUS is shared across fork.
//   6. Writing to a PROT_READ mapping kills the writer.
//   7. Touching a mapping past the object's size kills the toucher.
//   8. ftruncate down while mapped is EBUSY; after munmap it works, and
//      growing back reads zeros.
//   9. The mapping bound: 200 mappings of one object in total, then mmap
//      is ENOMEM and fork fails.
//  10. No leak: 100 create/map/fork/write/release cycles leave MemFree
//      where it was.
//  11. SMP: two processes pass a counter back and forth through shared
//      memory 20000 times without losing a step.
#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/wait.h>

#define PAGE 4096
static int fails;

static void check(int ok, const char *what) {
    printf("%s: %s\n", what, ok ? "PASS" : "FAIL");
    if (!ok) fails++;
}

static int new_memfd(size_t size) {
    int fd = memfd_create("shm_test", MFD_CLOEXEC);
    if (fd < 0) return -1;
    if (ftruncate(fd, size) != 0) { close(fd); return -1; }
    return fd;
}

// This kernel has no pread/pwrite syscalls.
static ssize_t pread_(int fd, void *buf, size_t n, off_t off) {
    return lseek(fd, off, SEEK_SET) == off ? read(fd, buf, n) : -1;
}
static ssize_t pwrite_(int fd, const void *buf, size_t n, off_t off) {
    return lseek(fd, off, SEEK_SET) == off ? write(fd, buf, n) : -1;
}

static char *map(int fd, size_t len, int prot) {
    void *p = mmap(NULL, len, prot, MAP_SHARED, fd, 0);
    return p == MAP_FAILED ? NULL : p;
}

// Child exit status: 0 if it exited 0, -sig if a signal killed it.
static int wait_child(pid_t pid) {
    int st = 0;
    if (waitpid(pid, &st, 0) != pid) return 1000;
    if (WIFSIGNALED(st)) return -WTERMSIG(st);
    return WIFEXITED(st) ? WEXITSTATUS(st) : 1001;
}

static void case1(void) {
    int fd = new_memfd(3 * PAGE);
    char *p = fd >= 0 ? map(fd, 3 * PAGE, PROT_READ | PROT_WRITE) : NULL;
    if (!p) { check(0, "1 memfd+mmap"); return; }
    strcpy(p + PAGE + 10, "via mapping");
    char buf[16] = {0};
    int ok = pread_(fd, buf, 11, PAGE + 10) == 11 && strcmp(buf, "via mapping") == 0;
    ok &= pwrite_(fd, "via fd", 7, 2 * PAGE) == 7 && strcmp(p + 2 * PAGE, "via fd") == 0;
    struct stat st;
    ok &= fstat(fd, &st) == 0 && st.st_size == 3 * PAGE;
    ok &= p[0] == 0 && p[3 * PAGE - 1] == 0; // untouched pages are zeros
    check(ok, "1 memfd+mmap: mapping and fd agree, fstat size");
    munmap(p, 3 * PAGE);
    close(fd);
}

static void case2(void) {
    int fd = new_memfd(PAGE);
    volatile char *p = fd >= 0 ? (volatile char *)map(fd, PAGE, PROT_READ | PROT_WRITE) : NULL;
    if (!p) { check(0, "2 fork"); return; }
    p[0] = 'P'; // mapped before fork, so fork has a present page to share
    pid_t pid = fork();
    if (pid == 0) {
        p[1] = 'C';                           // child → parent
        while (p[2] != 'p') sched_yield();    // parent → child
        _exit(p[0] == 'P' ? 0 : 1);
    }
    while (p[1] != 'C') sched_yield();
    p[2] = 'p';
    check(wait_child(pid) == 0, "2 fork: writes cross both ways (no COW)");
    munmap((void *)p, PAGE);
    close(fd);
}

static int send_fd(int sock, int fd) {
    char byte = 'x';
    struct iovec iov = { &byte, 1 };
    char ctl[CMSG_SPACE(sizeof(int))];
    struct msghdr msg = {0};
    msg.msg_iov = &iov; msg.msg_iovlen = 1;
    msg.msg_control = ctl; msg.msg_controllen = sizeof ctl;
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &fd, sizeof(int));
    return sendmsg(sock, &msg, 0) == 1 ? 0 : -1;
}

static int recv_fd(int sock) {
    char byte;
    struct iovec iov = { &byte, 1 };
    char ctl[CMSG_SPACE(sizeof(int))];
    struct msghdr msg = {0};
    msg.msg_iov = &iov; msg.msg_iovlen = 1;
    msg.msg_control = ctl; msg.msg_controllen = sizeof ctl;
    if (recvmsg(sock, &msg, 0) != 1) return -1;
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    if (!c || c->cmsg_type != SCM_RIGHTS) return -1;
    int fd;
    memcpy(&fd, CMSG_DATA(c), sizeof(int));
    return fd;
}

static void case3(void) {
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) { check(0, "3 SCM_RIGHTS"); return; }
    pid_t pid = fork();
    if (pid == 0) {
        close(sv[0]);
        int fd = recv_fd(sv[1]);
        char *p = fd >= 0 ? map(fd, PAGE, PROT_READ | PROT_WRITE) : NULL;
        if (!p) _exit(2);
        strcpy(p, "from receiver");
        char done = 1;
        write(sv[1], &done, 1);
        _exit(0);
    }
    close(sv[1]);
    // Created after the fork: the child can only have it through the socket.
    int fd = new_memfd(PAGE);
    char *p = fd >= 0 ? map(fd, PAGE, PROT_READ | PROT_WRITE) : NULL;
    char done = 0;
    int ok = p && send_fd(sv[0], fd) == 0 && read(sv[0], &done, 1) == 1;
    ok &= wait_child(pid) == 0 && p && strcmp(p, "from receiver") == 0;
    check(ok, "3 SCM_RIGHTS: an unrelated receiver maps it and writes");
    if (p) munmap(p, PAGE);
    close(fd);
    close(sv[0]);
}

static void case4(void) {
    int fd = new_memfd(PAGE);
    char *p = fd >= 0 ? map(fd, PAGE, PROT_READ | PROT_WRITE) : NULL;
    if (!p) { check(0, "4 lifetimes"); return; }
    strcpy(p, "kept");
    close(fd);
    int ok = strcmp(p, "kept") == 0;
    strcpy(p, "still");
    ok &= strcmp(p, "still") == 0;
    munmap(p, PAGE);

    fd = new_memfd(PAGE);
    p = map(fd, PAGE, PROT_READ | PROT_WRITE);
    strcpy(p, "in the fd");
    munmap(p, PAGE);
    char buf[10] = {0};
    ok &= pread_(fd, buf, 9, 0) == 9 && strcmp(buf, "in the fd") == 0;
    close(fd);
    check(ok, "4 mapping outlives fd, fd outlives mapping");
}

static void case5(void) {
    volatile int *p = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) { check(0, "5 MAP_SHARED|MAP_ANONYMOUS"); return; }
    pid_t pid = fork();
    if (pid == 0) { p[0] = 42; _exit(0); }
    int ok = wait_child(pid) == 0 && p[0] == 42;
    check(ok, "5 MAP_SHARED|MAP_ANONYMOUS shared across fork");
    munmap((void *)p, PAGE);
}

static void case6_7(void) {
    int fd = new_memfd(PAGE);
    pid_t pid = fork();
    if (pid == 0) {
        char *p = map(fd, PAGE, PROT_READ);
        if (!p) _exit(2);
        volatile char c = p[0]; (void)c; // reading is fine
        p[0] = 1;                         // writing is not
        _exit(0);
    }
    int r = wait_child(pid);
    check(r < 0, "6 write to a PROT_READ mapping kills the writer");

    pid = fork();
    if (pid == 0) {
        char *p = map(fd, 2 * PAGE, PROT_READ | PROT_WRITE);
        if (!p) _exit(2);
        p[0] = 1;       // inside the object
        p[PAGE] = 1;    // past its size
        _exit(0);
    }
    r = wait_child(pid);
    check(r < 0, "7 touching past the object's size kills the toucher");
    close(fd);
}

static void case8(void) {
    int fd = new_memfd(2 * PAGE);
    char *p = map(fd, 2 * PAGE, PROT_READ | PROT_WRITE);
    if (!p) { check(0, "8 ftruncate"); return; }
    memset(p, 'x', 2 * PAGE);
    int ok = ftruncate(fd, PAGE) == -1 && errno == EBUSY;
    munmap(p, 2 * PAGE);
    ok &= ftruncate(fd, 100) == 0;
    ok &= ftruncate(fd, 2 * PAGE) == 0;
    p = map(fd, 2 * PAGE, PROT_READ);
    ok &= p && p[99] == 'x' && p[100] == 0 && p[PAGE] == 0;
    struct stat st;
    ok &= fstat(fd, &st) == 0 && st.st_size == 2 * PAGE;
    if (p) munmap(p, 2 * PAGE);
    close(fd);
    check(ok, "8 shrink mapped: EBUSY; shrink unmapped + grow: zeros");
}

#define PER_PROC 40
static void case9(void) {
    int fd = new_memfd(PAGE);
    char *maps[PER_PROC];
    int ok = fd >= 0;
    for (int i = 0; ok && i < PER_PROC; i++) ok = (maps[i] = map(fd, PAGE, PROT_READ)) != NULL;
    if (!ok) { check(0, "9 mapping bound: setup"); return; }
    // The children just wait to be killed. (Not blocked reading one shared
    // pipe: a pipe here keeps a single waiting reader, and the others would
    // never wake — a separate kernel bug.)
    pid_t kids[4];
    int forked = 0;
    for (int i = 0; i < 4; i++) {           // 40 + 4*40 = 200
        pid_t pid = fork();
        if (pid == 0) { for (;;) sleep(1); }
        if (pid < 0) break;
        kids[forked++] = pid;
    }
    ok = forked == 4;
    void *extra = mmap(NULL, PAGE, PROT_READ, MAP_SHARED, fd, 0);
    ok &= extra == MAP_FAILED && errno == ENOMEM;
    pid_t over = fork();
    if (over == 0) _exit(0);
    ok &= over < 0;
    if (over > 0) wait_child(over);
    for (int i = 0; i < forked; i++) { kill(kids[i], SIGKILL); wait_child(kids[i]); }
    // With the children gone, the bound has room again.
    extra = mmap(NULL, PAGE, PROT_READ, MAP_SHARED, fd, 0);
    ok &= extra != MAP_FAILED;
    if (extra != MAP_FAILED) munmap(extra, PAGE);
    for (int i = 0; i < PER_PROC; i++) munmap(maps[i], PAGE);
    close(fd);
    check(ok, "9 200 mappings, then mmap ENOMEM and fork fails");
}

static long mem_free_kb(void) {
    FILE *f = fopen("/proc/meminfo", "r");
    if (!f) return -1;
    char line[128];
    long kb = -1;
    while (fgets(line, sizeof line, f))
        if (sscanf(line, "MemFree: %ld kB", &kb) == 1) break;
    fclose(f);
    return kb;
}

static void cycle(void) {
    int fd = new_memfd(16 * PAGE);
    char *p = map(fd, 16 * PAGE, PROT_READ | PROT_WRITE);
    if (!p) return;
    memset(p, 1, 16 * PAGE);
    pid_t pid = fork();
    if (pid == 0) { p[5 * PAGE] = 2; _exit(0); }
    wait_child(pid);
    munmap(p, 16 * PAGE);
    close(fd);
}

static void case10(void) {
    cycle(); // warm up any cache the first cycle fills
    long before = mem_free_kb();
    for (int i = 0; i < 100; i++) cycle();
    long after = mem_free_kb();
    // 100 cycles of 64 KiB objects: a leak of the objects alone is 6400 kB.
    int ok = before > 0 && after > 0 && before - after < 1024;
    printf("  MemFree before=%ld kB after=%ld kB\n", before, after);
    check(ok, "10 no leak over 100 create/map/fork/release cycles");
}

#define ROUNDS 20000
static void case11(void) {
    volatile int *turn = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (turn == MAP_FAILED) { check(0, "11 SMP ping-pong"); return; }
    *turn = 0;
    pid_t pid = fork();
    if (pid == 0) {
        for (int i = 1; i < 2 * ROUNDS; i += 2) {
            while (__atomic_load_n(turn, __ATOMIC_ACQUIRE) != i - 1) sched_yield();
            __atomic_store_n(turn, i, __ATOMIC_RELEASE);
        }
        _exit(0);
    }
    for (int i = 2; i <= 2 * ROUNDS; i += 2) {
        while (__atomic_load_n(turn, __ATOMIC_ACQUIRE) != i - 1) sched_yield();
        __atomic_store_n(turn, i, __ATOMIC_RELEASE);
    }
    int ok = wait_child(pid) == 0 && *turn == 2 * ROUNDS;
    check(ok, "11 SMP ping-pong through shared memory");
    munmap((void *)turn, PAGE);
}

int main(void) {
    case1();
    case2();
    case3();
    case4();
    case5();
    case6_7();
    case8();
    case9();
    case10();
    case11();
    printf("shm_test: %s (%d failed)\n", fails ? "FAIL" : "PASS", fails);
    return fails ? 1 : 0;
}
