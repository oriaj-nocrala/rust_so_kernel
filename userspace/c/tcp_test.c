// userspace/c/tcp_test.c
//
// End-to-end AF_INET/SOCK_STREAM test through mlibc and the syscall layer.
// It needs a peer on the host, because QEMU's user network has no loopback:
//
//   tcp_test [echo_port]   client: connect to the host's echo server at
//                          10.0.2.2:<echo_port> (default 47001), which sends
//                          "HELLO FROM HOST\n" then echoes everything back
//   tcp_test serve [port]  server: listen (default 7777), accept one
//                          connection (QEMU `hostfwd` brings the host in),
//                          expect "ping from host", answer "pong", wait for EOF
//
// Prints one line per check, then PASS/FAIL.

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static int failures = 0;

static void check(int ok, const char *what) {
    printf("%s %s\n", ok ? "  ok  " : "  FAIL", what);
    if (!ok) failures++;
}

static struct sockaddr_in addr_of(const char *ip, unsigned port) {
    struct sockaddr_in a;
    memset(&a, 0, sizeof a);
    a.sin_family = AF_INET;
    a.sin_port = htons(port);
    inet_pton(AF_INET, ip, &a.sin_addr);
    return a;
}

/* connect(), retrying while DHCP has not configured the interface yet. */
static int connect_retry(int s, const struct sockaddr_in *a, int *err) {
    for (int i = 0; i < 100; i++) {
        if (connect(s, (const struct sockaddr *)a, sizeof *a) == 0) return 0;
        *err = errno;
        if (errno != ENETUNREACH) return -1;
        usleep(100 * 1000);
    }
    return -1;
}

static int read_all(int s, char *buf, size_t want) {
    size_t got = 0;
    while (got < want) {
        ssize_t n = read(s, buf + got, want - got);
        if (n <= 0) return -1;
        got += (size_t)n;
    }
    return 0;
}

static void client(unsigned port) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    check(s >= 0, "socket(AF_INET, SOCK_STREAM)");

    int ty = 0;
    socklen_t tl = sizeof ty;
    check(getsockopt(s, SOL_SOCKET, SO_TYPE, &ty, &tl) == 0 && ty == SOCK_STREAM, "SO_TYPE is SOCK_STREAM");

    char buf[4096];
    errno = 0;
    check(read(s, buf, sizeof buf) < 0 && errno == ENOTCONN, "read on an unconnected socket -> ENOTCONN");

    struct sockaddr_in host = addr_of("10.0.2.2", port);
    int err = 0;
    check(connect_retry(s, &host, &err) == 0, "blocking connect() to the host echo server");

    struct sockaddr_in me, peer;
    socklen_t l = sizeof me;
    check(getsockname(s, (struct sockaddr *)&me, &l) == 0 && ntohs(me.sin_port) >= 49152 && me.sin_addr.s_addr != 0, "getsockname: ephemeral port, real address");
    l = sizeof peer;
    check(getpeername(s, (struct sockaddr *)&peer, &l) == 0 && peer.sin_addr.s_addr == host.sin_addr.s_addr && ntohs(peer.sin_port) == port, "getpeername is the host");
    int soerr = -1;
    tl = sizeof soerr;
    check(getsockopt(s, SOL_SOCKET, SO_ERROR, &soerr, &tl) == 0 && soerr == 0, "SO_ERROR is 0");

    const char banner[] = "HELLO FROM HOST\n";
    char got[64];
    check(read_all(s, got, sizeof banner - 1) == 0 && memcmp(got, banner, sizeof banner - 1) == 0, "banner arrives (blocking read)");

    /* 120 KB out and back: more than a socket buffer, so poll() drives both
     * directions to keep the echo from deadlocking. */
    enum { N = 120000 };
    unsigned char *out = malloc(N), *in = malloc(N);
    for (int i = 0; i < N; i++) out[i] = (unsigned char)(i * 13 + 5);
    size_t sent = 0, recvd = 0;
    int ok = 1;
    for (int spin = 0; ok && recvd < N && spin < 100000; spin++) {
        struct pollfd p = { .fd = s, .events = POLLIN | (sent < N ? POLLOUT : 0) };
        if (poll(&p, 1, 10000) != 1) { ok = 0; break; }
        if (p.revents & POLLOUT) {
            ssize_t n = write(s, out + sent, N - sent > 8192 ? 8192 : N - sent);
            if (n < 0) { ok = 0; break; }
            sent += (size_t)n;
        }
        if (p.revents & POLLIN) {
            ssize_t n = read(s, in + recvd, N - recvd);
            if (n <= 0) { ok = 0; break; }
            recvd += (size_t)n;
        }
    }
    check(ok && sent == N && recvd == N && memcmp(in, out, N) == 0, "120 KB echoed back intact under poll()");

    check(shutdown(s, SHUT_WR) == 0, "shutdown(SHUT_WR)");
    check(read(s, buf, sizeof buf) == 0, "read returns 0 (EOF) once the host closes");
    errno = 0;
    check(write(s, "x", 1) < 0 && errno == EPIPE, "write after shutdown -> EPIPE");
    close(s);

    /* Nothing listens on host port 1: the connect must fail, not hang. */
    int r = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in closed = addr_of("10.0.2.2", 1);
    errno = 0;
    check(connect(r, (struct sockaddr *)&closed, sizeof closed) < 0 && errno == ECONNREFUSED, "connect to a closed port -> ECONNREFUSED");
    close(r);

    /* Non-blocking connect: EINPROGRESS, then poll for writability, SO_ERROR 0. */
    int nb = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    errno = 0;
    int c = connect(nb, (struct sockaddr *)&host, sizeof host);
    check(c < 0 && errno == EINPROGRESS, "non-blocking connect -> EINPROGRESS");
    struct pollfd p = { .fd = nb, .events = POLLOUT };
    check(poll(&p, 1, 10000) == 1 && (p.revents & POLLOUT), "poll(POLLOUT) when the handshake completes");
    soerr = -1;
    tl = sizeof soerr;
    check(getsockopt(nb, SOL_SOCKET, SO_ERROR, &soerr, &tl) == 0 && soerr == 0, "SO_ERROR 0 after a good non-blocking connect");
    /* The host sends its banner at once: take it, then nothing is left. */
    struct pollfd pi = { .fd = nb, .events = POLLIN };
    check(poll(&pi, 1, 10000) == 1 && recv(nb, buf, sizeof buf, MSG_DONTWAIT) == 16, "non-blocking recv gets the banner");
    errno = 0;
    check(recv(nb, buf, sizeof buf, MSG_DONTWAIT) < 0 && errno == EAGAIN, "then recv with nothing queued -> EAGAIN");
    close(nb);
}

static void server(unsigned port) {
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in any = addr_of("0.0.0.0", port);
    check(bind(l, (struct sockaddr *)&any, sizeof any) == 0, "bind the listener");
    check(listen(l, 4) == 0, "listen");
    struct pollfd p = { .fd = l, .events = POLLIN };
    check(poll(&p, 1, 40000) == 1 && (p.revents & POLLIN), "poll(POLLIN) on the listener when a client arrives");
    struct sockaddr_in from;
    socklen_t fl = sizeof from;
    int c = accept(l, (struct sockaddr *)&from, &fl);
    check(c >= 0 && from.sin_family == AF_INET, "accept returns a connected socket and the peer address");
    char buf[64];
    check(read_all(c, buf, 14) == 0 && memcmp(buf, "ping from host", 14) == 0, "request arrives");
    check(write(c, "pong", 4) == 4, "reply written");
    check(read(c, buf, sizeof buf) == 0, "EOF when the client closes");
    close(c);
    close(l);
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "serve") == 0)
        server(argc > 2 ? (unsigned)atoi(argv[2]) : 7777);
    else
        client(argc > 1 ? (unsigned)atoi(argv[1]) : 47001);
    printf(failures ? "tcp_test: FAIL (%d)\n" : "tcp_test: PASS\n", failures);
    return failures != 0;
}
