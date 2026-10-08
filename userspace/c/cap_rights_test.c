// Capability rights on descriptors (cap_rights_limit 405 / cap_rights_get 406): a fresh fd has every right; limiting only
// narrows; each syscall refuses with ENOTCAPABLE without its right (read/write/seek/fstat/fcntl/mmap/poll, the *at family
// through a dirfd); the mask survives dup, dup2, F_DUPFD, fork, exec and SCM_RIGHTS; a file opened through a dirfd and an
// accepted socket get the parent descriptor's rights; an absolute path does not use the dirfd at all. Raw syscalls.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <stdlib.h>
#include <unistd.h>
#include <fcntl.h>
#include <poll.h>
#include <sys/wait.h>
#include <sys/stat.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include "constanos_capsicum.h"

static long sc(long nr, long a, long b, long c) {
    long ret;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(nr), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return ret;
}
static long sc6(long nr, long a, long b, long c, long d, long e, long f) {
    long ret;
    register long r10 __asm__("r10") = d;
    register long r8 __asm__("r8") = e;
    register long r9 __asm__("r9") = f;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8), "r"(r9) : "rcx", "r11", "memory");
    return ret;
}

enum {
    SYS_read = 0, SYS_write = 1, SYS_close = 3, SYS_fstat = 5, SYS_poll = 7, SYS_lseek = 8, SYS_mmap = 9,
    SYS_dup = 32, SYS_dup2 = 33, SYS_socket = 41, SYS_connect = 42, SYS_accept = 43, SYS_sendmsg = 46, SYS_recvmsg = 47,
    SYS_bind = 49, SYS_listen = 50, SYS_socketpair = 53, SYS_fcntl = 72, SYS_ftruncate = 77, SYS_openat = 257,
    SYS_mkdirat = 258, SYS_newfstatat = 262, SYS_unlinkat = 263, SYS_memfd_create = 319, SYS_openat2 = 437,
    AT_FDCWD_ = -100, AT_EMPTY_PATH_ = 0x1000,
};

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

static cap_rights_t rights_of(long fd) {
    cap_rights_t r = 0xdead;
    if (cap_rights_get((int)fd, &r) != 0) return 0xbad;
    return r;
}

static long limit(long fd, cap_rights_t r) { return sc(SYS_cap_rights_limit, fd, (long)r, 0); }

int main(int argc, char **argv) {
    if (argc == 3 && !strcmp(argv[1], "exec-child")) {
        // After exec: the fd still has only CAP_READ.
        long fd = atol(argv[2]);
        char c;
        int ok = rights_of(fd) == CAP_READ && sc(SYS_read, fd, (long)&c, 1) >= 0 && sc(SYS_write, fd, (long)"x", 1) == -ENOTCAPABLE;
        return ok ? 42 : 43;
    }

    mkdir("/tmp/crt", 0755);
    int w = open("/tmp/crt/f", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    write(w, "hello", 5);
    close(w);
    unlink("/tmp/crt/g");
    rmdir("/tmp/crt/sub");

    printf("limit and the plain calls\n");
    long fd = sc(SYS_openat, AT_FDCWD_, (long)"/tmp/crt/f", O_RDWR);
    CHECK(rights_of(fd) == CAP_ALL, "a fresh fd has every right (%#llx)", (unsigned long long)rights_of(fd));
    CHECK(limit(fd, CAP_READ | CAP_SEEK) == 0, "limit to READ|SEEK");
    CHECK(rights_of(fd) == (CAP_READ | CAP_SEEK), "cap_rights_get says so");
    char buf[8] = {0};
    CHECK(sc(SYS_read, fd, (long)buf, 5) == 5 && !memcmp(buf, "hello", 5), "read works");
    CHECK(sc(SYS_lseek, fd, 0, SEEK_SET) == 0, "lseek works");
    CHECK(sc(SYS_write, fd, (long)"x", 1) == -ENOTCAPABLE, "write: ENOTCAPABLE");
    CHECK(sc(SYS_ftruncate, fd, 0, 0) == -ENOTCAPABLE, "ftruncate: ENOTCAPABLE");
    struct stat st;
    CHECK(sc(SYS_fstat, fd, (long)&st, 0) == -ENOTCAPABLE, "fstat: ENOTCAPABLE");
    CHECK(sc6(SYS_newfstatat, fd, (long)"", (long)&st, AT_EMPTY_PATH_, 0, 0) == -ENOTCAPABLE, "fstatat(AT_EMPTY_PATH): ENOTCAPABLE");
    CHECK(sc(SYS_fcntl, fd, F_GETFL, 0) == -ENOTCAPABLE, "fcntl F_GETFL: ENOTCAPABLE");
    CHECK(sc(SYS_fcntl, fd, F_GETFD, 0) >= 0, "fcntl F_GETFD needs nothing");
    CHECK(limit(fd, CAP_READ | CAP_SEEK | CAP_WRITE) == -ENOTCAPABLE, "widening: ENOTCAPABLE");
    CHECK(rights_of(fd) == (CAP_READ | CAP_SEEK), "and nothing changed");
    CHECK(limit(fd, 1ull << 40) == -EINVAL, "an unknown bit: EINVAL");
    CHECK(limit(999, CAP_READ) == -EBADF, "a closed fd: EBADF");
    CHECK(limit(fd, CAP_READ) == 0, "narrowing again works");
    CHECK(sc(SYS_lseek, fd, 0, SEEK_SET) == -ENOTCAPABLE, "now lseek: ENOTCAPABLE");

    printf("dup, dup2, F_DUPFD, fork, exec keep the mask\n");
    long d1 = sc(SYS_dup, fd, 0, 0);
    CHECK(rights_of(d1) == CAP_READ, "dup (%#llx)", (unsigned long long)rights_of(d1));
    CHECK(sc(SYS_dup2, fd, 77, 0) == 77 && rights_of(77) == CAP_READ, "dup2");
    long d3 = sc(SYS_fcntl, fd, F_DUPFD, 100);
    CHECK(d3 >= 100 && rights_of(d3) == CAP_READ, "F_DUPFD");
    sc(SYS_close, 77, 0, 0);
    sc(SYS_close, d3, 0, 0);
    sc(SYS_close, d1, 0, 0);
    long reused = sc(SYS_openat, AT_FDCWD_, (long)"/tmp/crt/f", O_RDONLY);
    CHECK(rights_of(reused) == CAP_ALL, "a slot reused after close starts with every right");
    sc(SYS_close, reused, 0, 0);
    pid_t pid = fork();
    if (pid == 0) _exit(rights_of(fd) == CAP_READ && sc(SYS_write, fd, (long)"x", 1) == -ENOTCAPABLE ? 42 : 43);
    int status = 0;
    waitpid(pid, &status, 0);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 42, "fork: the child's fd is limited too (status %#x)", status);
    pid = fork();
    if (pid == 0) {
        char num[16];
        snprintf(num, sizeof num, "%ld", fd);
        char *args[] = { argv[0], "exec-child", num, NULL };
        execv("/mnt/bin/cap_rights_test", args);
        _exit(44);
    }
    waitpid(pid, &status, 0);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 42, "exec: the new image's fd is limited too (status %#x)", status);

    printf("SCM_RIGHTS keeps the mask\n");
    {
        int sv[2];
        CHECK(sc6(SYS_socketpair, 1 /*AF_UNIX*/, 1 /*SOCK_STREAM*/, 0, (long)sv, 0, 0) == 0, "socketpair");
        char byte = 'x';
        struct iovec iov = { &byte, 1 };
        union { struct cmsghdr h; char b[CMSG_SPACE(sizeof(int))]; } cbuf;
        struct msghdr m = {0};
        m.msg_iov = &iov; m.msg_iovlen = 1;
        m.msg_control = cbuf.b; m.msg_controllen = sizeof cbuf.b;
        struct cmsghdr *c = CMSG_FIRSTHDR(&m);
        c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS; c->cmsg_len = CMSG_LEN(sizeof(int));
        int sent = (int)fd;
        memcpy(CMSG_DATA(c), &sent, sizeof sent);
        CHECK(sc(SYS_sendmsg, sv[0], (long)&m, 0) == 1, "sendmsg with the limited fd");
        struct msghdr r = {0};
        char got;
        struct iovec riov = { &got, 1 };
        union { struct cmsghdr h; char b[CMSG_SPACE(sizeof(int))]; } rbuf;
        r.msg_iov = &riov; r.msg_iovlen = 1;
        r.msg_control = rbuf.b; r.msg_controllen = sizeof rbuf.b;
        CHECK(sc(SYS_recvmsg, sv[1], (long)&r, 0) == 1, "recvmsg");
        int recvd = -1;
        memcpy(&recvd, CMSG_DATA(CMSG_FIRSTHDR(&r)), sizeof recvd);
        CHECK(recvd >= 0 && recvd != sent && rights_of(recvd) == CAP_READ, "the received fd %d has CAP_READ only (%#llx)", recvd, (unsigned long long)rights_of(recvd));
        CHECK(sc(SYS_write, recvd, (long)"x", 1) == -ENOTCAPABLE, "and cannot write");
        sc(SYS_close, recvd, 0, 0);

        printf("socket rights\n");
        CHECK(limit(sv[0], CAP_READ) == 0, "limit a socket to CAP_READ");
        CHECK(sc(SYS_sendmsg, sv[0], (long)&m, 0) == -ENOTCAPABLE, "sendmsg: ENOTCAPABLE");
        CHECK(sc(SYS_write, sv[0], (long)"x", 1) == -ENOTCAPABLE, "write: ENOTCAPABLE");
        struct pollfd p = { sv[0], POLLIN, 0 };
        CHECK(sc(SYS_poll, (long)&p, 1, 0) == 1 && p.revents == POLLNVAL, "poll without CAP_EVENT: POLLNVAL (revents %#x)", p.revents);
        sc(SYS_close, sv[0], 0, 0);
        sc(SYS_close, sv[1], 0, 0);
    }
    {
        // accept: the new socket has the listening one's rights.
        unlink("/tmp/crt/sock");
        struct { unsigned short fam; char path[108]; } addr = { 1, "/tmp/crt/sock" };
        long ls = sc(SYS_socket, 1, 1, 0);
        CHECK(sc(SYS_bind, ls, (long)&addr, sizeof addr) == 0 && sc(SYS_listen, ls, 4, 0) == 0, "bind + listen");
        cap_rights_t lr = CAP_ACCEPT | CAP_READ | CAP_EVENT;
        CHECK(limit(ls, lr) == 0, "limit the listener to ACCEPT|READ|EVENT");
        CHECK(sc(SYS_listen, ls, 4, 0) == -ENOTCAPABLE, "listen again: ENOTCAPABLE");
        long cs = sc(SYS_socket, 1, 1, 0);
        CHECK(sc(SYS_connect, cs, (long)&addr, sizeof addr) == 0, "connect");
        long as = sc(SYS_accept, ls, 0, 0);
        CHECK(as >= 0 && rights_of(as) == lr, "the accepted socket has the listener's rights (%#llx)", (unsigned long long)rights_of(as));
        CHECK(sc(SYS_write, as, (long)"x", 1) == -ENOTCAPABLE, "so it cannot write");
        sc(SYS_close, as, 0, 0); sc(SYS_close, cs, 0, 0); sc(SYS_close, ls, 0, 0);
        unlink("/tmp/crt/sock");
    }
    {
        long mfd = sc(SYS_memfd_create, (long)"crt", 0, 0);
        sc(SYS_ftruncate, mfd, 4096, 0);
        CHECK(limit(mfd, CAP_READ | CAP_WRITE) == 0, "limit a memfd to READ|WRITE");
        long p = sc6(SYS_mmap, 0, 4096, 1 /*PROT_READ*/, 1 /*MAP_SHARED*/, mfd, 0);
        CHECK(p == -ENOTCAPABLE, "mmap without CAP_MMAP: ENOTCAPABLE (%ld)", p);
        long mfd2 = sc(SYS_memfd_create, (long)"crt2", 0, 0);
        sc(SYS_ftruncate, mfd2, 4096, 0);
        CHECK(limit(mfd2, CAP_MMAP | CAP_READ) == 0, "another limited to MMAP|READ");
        CHECK(sc6(SYS_mmap, 0, 4096, 1, 1, mfd2, 0) > 0, "PROT_READ shared map works");
        CHECK(sc6(SYS_mmap, 0, 4096, 3, 1, mfd2, 0) == -ENOTCAPABLE, "PROT_WRITE shared map needs CAP_WRITE");
        sc(SYS_close, mfd, 0, 0); sc(SYS_close, mfd2, 0, 0);
    }

    printf("dirfd rights\n");
    long d = sc(SYS_openat, AT_FDCWD_, (long)"/tmp/crt", O_RDONLY | O_DIRECTORY);
    CHECK(limit(d, CAP_LOOKUP | CAP_READ | CAP_FSTAT) == 0, "limit the dirfd to LOOKUP|READ|FSTAT");
    long f = sc(SYS_openat, d, (long)"f", O_RDONLY);
    CHECK(f >= 0, "openat(d, f, O_RDONLY): %ld", f);
    CHECK(rights_of(f) == (CAP_LOOKUP | CAP_READ | CAP_FSTAT), "the new fd has the dirfd's rights (%#llx)", (unsigned long long)rights_of(f));
    CHECK(sc(SYS_write, f, (long)"x", 1) == -ENOTCAPABLE, "so it cannot write");
    sc(SYS_close, f, 0, 0);
    CHECK(sc(SYS_openat, d, (long)"f", O_WRONLY) == -ENOTCAPABLE, "openat O_WRONLY: ENOTCAPABLE");
    CHECK(sc(SYS_openat, d, (long)"g", O_RDONLY | O_CREAT) == -ENOTCAPABLE, "openat O_CREAT: ENOTCAPABLE");
    CHECK(sc(SYS_mkdirat, d, (long)"sub", 0755) == -ENOTCAPABLE, "mkdirat: ENOTCAPABLE");
    CHECK(sc(SYS_unlinkat, d, (long)"f", 0) == -ENOTCAPABLE, "unlinkat: ENOTCAPABLE");
    CHECK(sc6(SYS_newfstatat, d, (long)"f", (long)&st, 0, 0, 0) == 0, "fstatat with CAP_FSTAT works");
    {
        struct { uint64_t flags, mode, resolve; } how = { O_WRONLY, 0, 8 /*RESOLVE_BENEATH*/ };
        CHECK(sc6(SYS_openat2, d, (long)"f", (long)&how, sizeof how, 0, 0) == -ENOTCAPABLE, "openat2 O_WRONLY: ENOTCAPABLE");
    }
    long abs = sc(SYS_openat, d, (long)"/tmp/crt/f", O_WRONLY);
    CHECK(abs >= 0 && rights_of(abs) == CAP_ALL, "an absolute path ignores the dirfd (rights %#llx)", (unsigned long long)rights_of(abs));
    sc(SYS_close, abs, 0, 0);
    CHECK(limit(d, CAP_READ) == 0, "drop CAP_LOOKUP");
    CHECK(sc(SYS_openat, d, (long)"f", O_RDONLY) == -ENOTCAPABLE, "now even a read-only openat: ENOTCAPABLE");
    sc(SYS_close, d, 0, 0);
    long full = sc(SYS_openat, AT_FDCWD_, (long)"/tmp/crt", O_RDONLY | O_DIRECTORY);
    CHECK(sc(SYS_mkdirat, full, (long)"sub", 0755) == 0, "with every right, mkdirat works");
    CHECK(sc(SYS_unlinkat, full, (long)"sub", 0x200) == 0, "and unlinkat(AT_REMOVEDIR)");
    sc(SYS_close, full, 0, 0);

    sc(SYS_close, fd, 0, 0);
    printf(failures ? "cap_rights_test: FAIL\n" : "cap_rights_test: PASS\n");
    return failures != 0;
}
