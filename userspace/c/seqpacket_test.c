// AF_UNIX SOCK_SEQPACKET: message boundaries through read/write and send/recv, truncation, EOF when the peer closes (the way Rust's
// std::process::Command reports a failed exec), blocking wakeups across a fork, poll, and listen/accept/connect by path.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <time.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>

#ifndef SOCK_SEQPACKET
#define SOCK_SEQPACKET 5
#endif
static int failures;
#define CHECK(cond, ...) do { if (!(cond)) { failures++; printf("  FAIL %s (line %d): ", #cond, __LINE__); printf(__VA_ARGS__); printf(" [errno %d]\n", errno); } } while (0)
static void ms(int n) { struct timespec ts = {n / 1000, (n % 1000) * 1000000L}; nanosleep(&ts, NULL); }

int main(void) {
    printf("seqpacket_test:\n");
    int sv[2];
    CHECK(socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sv) == 0, "socketpair");
    int type = 0; socklen_t tl = sizeof type;
    CHECK(getsockopt(sv[0], SOL_SOCKET, SO_TYPE, &type, &tl) == 0 && type == SOCK_SEQPACKET, "SO_TYPE is %d", type);

    // Boundaries: three writes are three reads, even with a big buffer.
    char buf[64];
    CHECK(write(sv[0], "abc", 3) == 3 && write(sv[0], "de", 2) == 2 && write(sv[0], "f", 1) == 1, "three writes");
    CHECK(read(sv[1], buf, sizeof buf) == 3 && !memcmp(buf, "abc", 3), "first read is the first message");
    CHECK(read(sv[1], buf, sizeof buf) == 2 && !memcmp(buf, "de", 2), "second read");
    CHECK(read(sv[1], buf, sizeof buf) == 1 && buf[0] == 'f', "third read");

    // Truncation: the rest of a message that does not fit is dropped (MSG_TRUNC says so).
    CHECK(send(sv[0], "0123456789", 10, 0) == 10, "send 10");
    CHECK(send(sv[0], "next", 4, 0) == 4, "send next");
    ssize_t n = recv(sv[1], buf, 4, MSG_TRUNC);
    CHECK(n == 10, "MSG_TRUNC returns the real length (%zd)", n);
    CHECK(recv(sv[1], buf, sizeof buf, 0) == 4 && !memcmp(buf, "next", 4), "the next message starts clean");

    // Non-blocking: nothing to read is EAGAIN, not EOF.
    CHECK(recv(sv[1], buf, sizeof buf, MSG_DONTWAIT) == -1 && errno == EAGAIN, "empty is EAGAIN");

    // poll
    struct pollfd p = { sv[1], POLLIN, 0 };
    CHECK(poll(&p, 1, 0) == 0, "not readable while empty");
    write(sv[0], "x", 1);
    CHECK(poll(&p, 1, 1000) == 1 && (p.revents & POLLIN), "readable once a message is queued");
    read(sv[1], buf, 1);

    // A blocked reader is woken by a message from another process; then EOF when that process exits.
    pid_t c = fork();
    if (c == 0) {
        close(sv[1]);
        ms(150);
        write(sv[0], "late", 4);
        ms(100);
        _exit(0);                                    // closes its end: EOF for the parent once the other end goes too
    }
    // (both ends are open in the parent; the parent's own sv[0] keeps the socket alive, so close it first)
    close(sv[0]);
    n = read(sv[1], buf, sizeof buf);
    CHECK(n == 4 && !memcmp(buf, "late", 4), "a blocked read gets the child's message (%zd)", n);
    n = read(sv[1], buf, sizeof buf);
    CHECK(n == 0, "EOF once every writer is gone (%zd)", n);
    CHECK(write(sv[1], "x", 1) == -1 && errno == EPIPE, "and a write is EPIPE");
    int st; waitpid(c, &st, 0);
    close(sv[1]);

    // The exec-failure protocol std uses: a CLOEXEC seqpacket end that closes on exec is EOF for the reader.
    CHECK(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, sv) == 0, "socketpair SOCK_CLOEXEC");
    c = fork();
    if (c == 0) {
        close(sv[0]);
        char *argv[] = {"busybox", "true", NULL};
        execv("/bin/busybox", argv);
        write(sv[1], "NOEX", 4);
        _exit(1);
    }
    close(sv[1]);
    n = read(sv[0], buf, sizeof buf);
    CHECK(n == 0, "a successful exec closes the CLOEXEC end: EOF (%zd)", n);
    waitpid(c, &st, 0);
    close(sv[0]);

    // listen/accept/connect by path
    int l = socket(AF_UNIX, SOCK_SEQPACKET, 0);
    struct sockaddr_un a = { .sun_family = AF_UNIX };
    strcpy(a.sun_path, "/tmp/seqpacket_test.sock");
    unlink(a.sun_path);
    CHECK(bind(l, (struct sockaddr *)&a, sizeof a) == 0 && listen(l, 4) == 0, "bind+listen");
    int cl = socket(AF_UNIX, SOCK_SEQPACKET, 0);
    CHECK(connect(cl, (struct sockaddr *)&a, sizeof a) == 0, "connect");
    int s = accept(l, NULL, NULL);
    CHECK(s >= 0, "accept");
    tl = sizeof type; type = 0;
    CHECK(getsockopt(s, SOL_SOCKET, SO_TYPE, &type, &tl) == 0 && type == SOCK_SEQPACKET, "accepted socket is SEQPACKET (%d)", type);
    send(cl, "one", 3, 0); send(cl, "two", 3, 0);
    CHECK(recv(s, buf, sizeof buf, 0) == 3 && !memcmp(buf, "one", 3), "message one");
    CHECK(recv(s, buf, sizeof buf, 0) == 3 && !memcmp(buf, "two", 3), "message two");
    // A stream client cannot connect to a seqpacket listener.
    int st_c = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(connect(st_c, (struct sockaddr *)&a, sizeof a) == -1, "stream to seqpacket is refused (errno %d)", errno);
    // Unsupported types stay unsupported.
    CHECK(socket(AF_UNIX, 3 /* SOCK_RAW */, 0) == -1, "SOCK_RAW is refused");
    close(s); close(cl); close(l); close(st_c);
    unlink(a.sun_path);

    printf(failures ? "seqpacket_test: %d FAILURES\n" : "seqpacket_test: OK\n", failures);
    return failures ? 1 : 0;
}
