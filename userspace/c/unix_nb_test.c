// Non-blocking AF_UNIX stream sockets the way mio/tokio drive them: SOCK_NONBLOCK|SOCK_CLOEXEC creation, listen(-1) (what Rust std passes),
// a connect() that completes at once (backlog has room), accept4 with flags, EAGAIN on an empty accept / read, and readiness
// reported through epoll on both ends.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/socket.h>
#include <sys/un.h>

#define printf(...) ((printf)(__VA_ARGS__), fflush(stdout))
static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

struct epoll_event { uint32_t events; uint64_t data; } __attribute__((packed));
#define EPOLLIN 1u
#define EPOLLOUT 4u
static long sc(long nr, long a, long b, long c, long d) {
    long r;
    register long r10 __asm__("r10") = d;
    __asm__ volatile("syscall" : "=a"(r) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10) : "rcx", "r11", "memory");
    return r;
}

static struct sockaddr_un addr_of(const char *path) {
    struct sockaddr_un a = {0};
    a.sun_family = AF_UNIX;
    strcpy(a.sun_path, path);
    return a;
}

int main(void) {
    const char *path = "/tmp/nb_test.sock";
    unlink(path);
    struct sockaddr_un sa = addr_of(path);

    printf("listener\n");
    int ls = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
    CHECK(ls >= 0, "socket errno %d", errno);
    CHECK((fcntl(ls, F_GETFL) & O_NONBLOCK) != 0, "SOCK_NONBLOCK not applied");
    CHECK((fcntl(ls, F_GETFD) & FD_CLOEXEC) != 0, "SOCK_CLOEXEC not applied");
    CHECK(bind(ls, (struct sockaddr *)&sa, sizeof sa) == 0, "bind errno %d", errno);
    CHECK(listen(ls, -1) == 0, "listen errno %d", errno);
    CHECK(accept4(ls, NULL, NULL, SOCK_NONBLOCK | SOCK_CLOEXEC) == -1 && errno == EAGAIN, "an empty accept gave errno %d, wanted EAGAIN", errno);

    printf("three non-blocking connects\n");
    int cl[3];
    int done = 0, first = 0;
    for (int i = 0; i < 3; i++) {
        cl[i] = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
        int r = connect(cl[i], (struct sockaddr *)&sa, sizeof sa);
        if (r == 0) done++; else if (!first) first = errno;
    }
    CHECK(done == 3, "%d of 3 connects completed (first errno %d)", done, first);

    printf("accept and exchange data\n");
    int srv[3];
    for (int i = 0; i < 3; i++) {
        srv[i] = accept4(ls, NULL, NULL, SOCK_NONBLOCK | SOCK_CLOEXEC);
        CHECK(srv[i] >= 0, "accept4 %d gave errno %d", i, errno);
    }
    CHECK(accept4(ls, NULL, NULL, 0) == -1 && errno == EAGAIN, "a 4th accept gave errno %d", errno);
    char buf[16];
    CHECK(read(srv[0], buf, 16) == -1 && errno == EAGAIN, "an empty non-blocking read gave errno %d", errno);
    CHECK(write(cl[0], "ping", 4) == 4, "write errno %d", errno);
    CHECK(read(srv[0], buf, 16) == 4 && memcmp(buf, "ping", 4) == 0, "read errno %d", errno);

    printf("epoll readiness on both ends\n");
    int ep = (int)sc(291, 0x80000, 0, 0, 0);
    struct epoll_event ev = { .events = EPOLLIN | EPOLLOUT, .data = 1 }, out[4];
    CHECK(sc(233, ep, 1, srv[1], (long)&ev) == 0, "ADD server end");
    ev.data = 2;
    CHECK(sc(233, ep, 1, cl[1], (long)&ev) == 0, "ADD client end");
    int n = (int)sc(232, ep, (long)out, 4, 0);
    int outs = 0;
    for (int i = 0; i < n; i++) if ((out[i].events & EPOLLOUT) && !(out[i].events & EPOLLIN)) outs++;
    CHECK(n == 2 && outs == 2, "fresh connection: %d events, %d writable-only", n, outs);
    write(cl[1], "x", 1);
    n = (int)sc(232, ep, (long)out, 4, 0);
    int readable = 0;
    for (int i = 0; i < n; i++) if (out[i].data == 1 && (out[i].events & EPOLLIN)) readable = 1;
    CHECK(readable, "the server end is not reported readable after a write (%d events)", n);
    close(cl[2]);
    CHECK(read(srv[2], buf, 16) == 0, "EOF after the peer closed");

    printf(failures ? "unix_nb_test: FAIL\n" : "unix_nb_test: PASS\n");
    return failures != 0;
}
