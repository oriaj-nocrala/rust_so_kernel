// userspace/c/socket_test.c
//
// End-to-end AF_UNIX test, written against the ordinary POSIX API through
// mlibc — so it exercises the whole stack at once: the sysdeps
// (mlibc-port/constanos-sysdeps/generic/generic.cpp), the ABI headers
// (abi-bits/socket.h, sys/un.h, errno.h), the syscall layer
// (kernel/src/process/syscall/ipc.rs) and the socket core (the host-tested
// `usock` crate).
//
// The host tests in `usock` already prove the state machines. What can only
// be proven here is that a real program, compiled against real headers, gets
// the behavior it expects across a real syscall boundary — which is exactly
// where this port's ABI constants have gone wrong before (SEEK_SET, O_CREAT,
// MAP_ANONYMOUS; see CLAUDE.md).
//
// Prints one line per check and a final PASS/FAIL.

#include <errno.h>
#include <fcntl.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/un.h>
#include <sys/wait.h>

static int failures = 0;

static void check(int ok, const char *what) {
    printf("%s %s\n", ok ? "  ok  " : "  FAIL", what);
    if (!ok) failures++;
}

static socklen_t fill_path(struct sockaddr_un *a, const char *path) {
    memset(a, 0, sizeof(*a));
    a->sun_family = AF_UNIX;
    strncpy(a->sun_path, path, sizeof(a->sun_path) - 1);
    return (socklen_t)(offsetof(struct sockaddr_un, sun_path) + strlen(path) + 1);
}

static socklen_t fill_abstract(struct sockaddr_un *a, const char *name) {
    memset(a, 0, sizeof(*a));
    a->sun_family = AF_UNIX;
    // Abstract namespace: a leading NUL, then the name verbatim.
    memcpy(a->sun_path + 1, name, strlen(name));
    return (socklen_t)(offsetof(struct sockaddr_un, sun_path) + 1 + strlen(name));
}

// ── 1. socketpair: the simplest connected pair ──────────────────────────

static void test_socketpair(void) {
    printf("socketpair:\n");
    int sv[2];
    check(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0, "socketpair() succeeds");

    check(write(sv[0], "ping", 4) == 4, "write() on a socket fd");
    char buf[16] = {0};
    check(read(sv[1], buf, sizeof(buf)) == 4 && memcmp(buf, "ping", 4) == 0,
          "read() sees it (plain read/write work on sockets)");

    check(send(sv[1], "pong", 4, 0) == 4, "send() back");
    memset(buf, 0, sizeof(buf));
    check(recv(sv[0], buf, sizeof(buf), 0) == 4 && memcmp(buf, "pong", 4) == 0,
          "recv() sees it");

    // Closing one end is the other's EOF.
    close(sv[1]);
    check(read(sv[0], buf, sizeof(buf)) == 0, "closed peer reads as EOF");
    check(send(sv[0], "x", 1, 0) < 0 && errno == EPIPE, "and sends as EPIPE");
    close(sv[0]);
}

// ── 2. datagrams keep message boundaries ────────────────────────────────

static void test_dgram(void) {
    printf("SOCK_DGRAM:\n");
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_DGRAM, 0, sv) != 0) {
        check(0, "socketpair(SOCK_DGRAM)");
        return;
    }
    check(1, "socketpair(SOCK_DGRAM) succeeds");

    send(sv[0], "one", 3, 0);
    send(sv[0], "two", 3, 0);

    char buf[16];
    ssize_t n = recv(sv[1], buf, sizeof(buf), 0);
    check(n == 3 && memcmp(buf, "one", 3) == 0, "first datagram arrives whole");
    n = recv(sv[1], buf, sizeof(buf), 0);
    check(n == 3 && memcmp(buf, "two", 3) == 0,
          "second is a separate message, not appended to the first");

    // A short read truncates and discards the remainder.
    send(sv[0], "abcdef", 6, 0);
    n = recv(sv[1], buf, 2, 0);
    check(n == 2, "a too-small recv() truncates");
    n = recv(sv[1], buf, sizeof(buf), MSG_DONTWAIT);
    check(n < 0 && errno == EAGAIN, "the truncated remainder is gone, not requeued");

    close(sv[0]);
    close(sv[1]);
}

// ── 3. named server: bind/listen/accept over a filesystem path ──────────

static void test_pathname_server(void) {
    printf("pathname server:\n");
    const char *path = "/tmp/socket_test.sock";
    unlink(path); // a leftover node from an earlier run is a real EADDRINUSE

    int srv = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un addr;
    socklen_t len = fill_path(&addr, path);
    check(srv >= 0, "socket()");
    check(bind(srv, (struct sockaddr *)&addr, len) == 0, "bind() to a path");

    // The node is real: it is visible to stat() as a socket.
    struct stat st;
    check(stat(path, &st) == 0 && S_ISSOCK(st.st_mode),
          "bind() created an S_IFSOCK node in the filesystem");

    // A second bind to the same name fails, because the node exists.
    int dup_sock = socket(AF_UNIX, SOCK_STREAM, 0);
    check(bind(dup_sock, (struct sockaddr *)&addr, len) < 0 && errno == EADDRINUSE,
          "a second bind() to the same path is EADDRINUSE");
    close(dup_sock);

    check(listen(srv, 4) == 0, "listen()");

    pid_t pid = fork();
    if (pid == 0) {
        int c = socket(AF_UNIX, SOCK_STREAM, 0);
        struct sockaddr_un ca;
        socklen_t cl = fill_path(&ca, path);
        if (connect(c, (struct sockaddr *)&ca, cl) != 0) _exit(1);
        write(c, "hello", 5);
        char r[16];
        read(c, r, sizeof(r));
        close(c);
        _exit(0);
    }

    int peer = accept(srv, NULL, NULL);
    check(peer >= 0, "accept() returns the connection");

    char buf[16] = {0};
    check(read(peer, buf, sizeof(buf)) == 5 && memcmp(buf, "hello", 5) == 0,
          "the client's bytes arrive");
    write(peer, "bye", 3);

    // getsockname() on the accepted socket reports the listener's address.
    struct sockaddr_un got;
    socklen_t gl = sizeof(got);
    check(getsockname(peer, (struct sockaddr *)&got, &gl) == 0
              && got.sun_family == AF_UNIX
              && strcmp(got.sun_path, path) == 0,
          "getsockname() reports the bound path");

    int status = 0;
    waitpid(pid, &status, 0);
    check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "client exited cleanly");

    close(peer);
    close(srv);
    check(unlink(path) == 0, "the socket node can be unlinked like any name");
}

// ── 4. abstract namespace: no filesystem node at all ────────────────────

static void test_abstract(void) {
    printf("abstract namespace:\n");
    int srv = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un addr;
    socklen_t len = fill_abstract(&addr, "socket-test-abstract");
    check(bind(srv, (struct sockaddr *)&addr, len) == 0, "bind() to an abstract name");
    check(listen(srv, 1) == 0, "listen()");

    pid_t pid = fork();
    if (pid == 0) {
        int c = socket(AF_UNIX, SOCK_STREAM, 0);
        struct sockaddr_un ca;
        socklen_t cl = fill_abstract(&ca, "socket-test-abstract");
        if (connect(c, (struct sockaddr *)&ca, cl) != 0) _exit(1);
        write(c, "abs", 3);
        close(c);
        _exit(0);
    }

    int peer = accept(srv, NULL, NULL);
    char buf[8] = {0};
    check(peer >= 0 && read(peer, buf, sizeof(buf)) == 3 && memcmp(buf, "abs", 3) == 0,
          "an abstract address connects with no node in the filesystem");

    int status = 0;
    waitpid(pid, &status, 0);
    close(peer);
    close(srv);
}

// ── 5. error cases userspace actually branches on ───────────────────────

static void test_errors(void) {
    printf("errors:\n");
    check(socket(AF_INET, SOCK_STREAM, 0) < 0 && errno == EAFNOSUPPORT,
          "socket(AF_INET) is EAFNOSUPPORT (there is no network stack)");

    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un a;
    socklen_t l = fill_path(&a, "/tmp/definitely-not-there.sock");
    check(connect(s, (struct sockaddr *)&a, l) < 0 && errno == ENOENT,
          "connect() to a missing path is ENOENT");

    char buf[4];
    check(recv(s, buf, sizeof(buf), 0) < 0 && errno == ENOTCONN,
          "recv() on an unconnected stream is ENOTCONN");
    check(listen(s, 1) < 0 && errno == EINVAL,
          "listen() without bind() is EINVAL");
    close(s);

    int d = socket(AF_UNIX, SOCK_DGRAM, 0);
    check(listen(d, 1) < 0 && errno == EOPNOTSUPP,
          "listen() on a datagram socket is EOPNOTSUPP");
    close(d);

    int f = open("/tmp", O_RDONLY);
    if (f >= 0) {
        check(listen(f, 1) < 0 && errno == ENOTSOCK,
              "a socket call on a non-socket fd is ENOTSOCK");
        close(f);
    }
}

// ── 6. socket options ───────────────────────────────────────────────────

static void test_sockopt(void) {
    printf("socket options:\n");
    int sv[2];
    socketpair(AF_UNIX, SOCK_DGRAM, 0, sv);

    int type = 0;
    socklen_t len = sizeof(type);
    check(getsockopt(sv[0], SOL_SOCKET, SO_TYPE, &type, &len) == 0 && type == SOCK_DGRAM,
          "SO_TYPE reports the type the socket was created with");

    int err = -1;
    len = sizeof(err);
    check(getsockopt(sv[0], SOL_SOCKET, SO_ERROR, &err, &len) == 0 && err == 0,
          "SO_ERROR is clear on a healthy socket");

    int want = 4096;
    check(setsockopt(sv[0], SOL_SOCKET, SO_RCVBUF, &want, sizeof(want)) == 0,
          "SO_RCVBUF can be set");
    int got = 0;
    len = sizeof(got);
    check(getsockopt(sv[0], SOL_SOCKET, SO_RCVBUF, &got, &len) == 0 && got == want,
          "and reads back");

    close(sv[0]);
    close(sv[1]);
}

// ── 7. half close ───────────────────────────────────────────────────────

static void test_shutdown(void) {
    printf("shutdown:\n");
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);

    check(shutdown(sv[0], SHUT_WR) == 0, "shutdown(SHUT_WR)");
    char buf[8];
    check(read(sv[1], buf, sizeof(buf)) == 0, "the peer sees EOF");
    check(send(sv[1], "still", 5, 0) == 5, "but the other direction still works");
    check(recv(sv[0], buf, sizeof(buf), 0) == 5, "and can still be read");

    close(sv[0]);
    close(sv[1]);
}

// ── 8. O_NONBLOCK via fcntl ─────────────────────────────────────────────

static void test_nonblock(void) {
    printf("O_NONBLOCK:\n");
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);

    // Blocking by default: prove it the only safe way (without hanging the
    // test) — set the flag, then check read() reports EAGAIN instead of
    // parking the process forever on an empty socket.
    check(fcntl(sv[0], F_SETFL, O_NONBLOCK) == 0, "fcntl(F_SETFL, O_NONBLOCK)");
    check((fcntl(sv[0], F_GETFL) & O_NONBLOCK) != 0, "and F_GETFL reads it back");

    char buf[8];
    check(read(sv[0], buf, sizeof(buf)) < 0 && errno == EAGAIN,
          "read() on an empty non-blocking socket is EAGAIN, not a block");

    write(sv[1], "now", 3);
    check(read(sv[0], buf, sizeof(buf)) == 3, "and still reads real data");

    close(sv[0]);
    close(sv[1]);
}

// ── 9. SCM_RIGHTS: passing an open file descriptor ──────────────────────

static void test_fd_passing(void) {
    printf("SCM_RIGHTS:\n");

    // Something worth passing: a file with known contents.
    const char *path = "/tmp/socket_test_payload.txt";
    FILE *f = fopen(path, "w");
    if (!f) { check(0, "could not create the payload file"); return; }
    fputs("passed-through", f);
    fclose(f);

    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);

    int fd = open(path, O_RDONLY);
    check(fd >= 0, "opened a file to send");

    // Send one byte of real data plus the descriptor: SCM_RIGHTS must ride
    // along with actual bytes, which is why the iovec is not empty.
    char data = 'x';
    struct iovec iov = { .iov_base = &data, .iov_len = 1 };
    char control[CMSG_SPACE(sizeof(int))];
    memset(control, 0, sizeof(control));

    struct msghdr msg;
    memset(&msg, 0, sizeof(msg));
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control;
    msg.msg_controllen = sizeof(control);

    struct cmsghdr *cmsg = CMSG_FIRSTHDR(&msg);
    cmsg->cmsg_level = SOL_SOCKET;
    cmsg->cmsg_type = SCM_RIGHTS;
    cmsg->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(cmsg), &fd, sizeof(int));

    check(sendmsg(sv[0], &msg, 0) == 1, "sendmsg() with SCM_RIGHTS");

    // Receive it into a different descriptor number.
    char rdata = 0;
    struct iovec riov = { .iov_base = &rdata, .iov_len = 1 };
    char rcontrol[CMSG_SPACE(sizeof(int))];
    memset(rcontrol, 0, sizeof(rcontrol));

    struct msghdr rmsg;
    memset(&rmsg, 0, sizeof(rmsg));
    rmsg.msg_iov = &riov;
    rmsg.msg_iovlen = 1;
    rmsg.msg_control = rcontrol;
    rmsg.msg_controllen = sizeof(rcontrol);

    check(recvmsg(sv[1], &rmsg, 0) == 1 && rdata == 'x', "recvmsg() gets the byte");

    struct cmsghdr *rc = CMSG_FIRSTHDR(&rmsg);
    int got_fd = -1;
    if (rc && rc->cmsg_level == SOL_SOCKET && rc->cmsg_type == SCM_RIGHTS) {
        memcpy(&got_fd, CMSG_DATA(rc), sizeof(int));
    }
    check(got_fd >= 0, "a descriptor came with it");
    check(got_fd != fd, "and it is a new descriptor number");

    if (got_fd >= 0) {
        char buf[32] = {0};
        ssize_t n = read(got_fd, buf, sizeof(buf) - 1);
        check(n == 14 && strcmp(buf, "passed-through") == 0,
              "the received descriptor reads the sender's file");
        close(got_fd);
    }

    // The sender still holds its own copy: passing duplicates, not moves.
    lseek(fd, 0, SEEK_SET);
    char again[32] = {0};
    check(read(fd, again, sizeof(again) - 1) == 14,
          "the sender's own descriptor is still open");

    close(fd);
    close(sv[0]);
    close(sv[1]);
    unlink(path);
}

int main(void) {
    printf("socket_test: AF_UNIX end-to-end\n");

    test_socketpair();
    test_dgram();
    test_pathname_server();
    test_abstract();
    test_errors();
    test_sockopt();
    test_shutdown();
    test_nonblock();
    test_fd_passing();

    if (failures == 0) {
        printf("socket_test: PASS\n");
        return 0;
    }
    printf("socket_test: FAIL (%d checks failed)\n", failures);
    return 1;
}
