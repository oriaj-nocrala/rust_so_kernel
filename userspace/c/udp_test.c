// userspace/c/udp_test.c
//
// End-to-end AF_INET/SOCK_DGRAM test through mlibc: sockaddr_in layout,
// bind/getsockname/connect/getpeername, EAGAIN and blocking behaviour, poll,
// and a real DNS round trip to QEMU's user-mode resolver (10.0.2.3) once
// DHCP has configured the interface. Prints one line per check, then
// PASS/FAIL.

#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
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

/* A DNS query for "example.com" A, id 0x4242, recursion desired. */
static const unsigned char QUERY[] = {
    0x42, 0x42, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    7, 'e', 'x', 'a', 'm', 'p', 'l', 'e', 3, 'c', 'o', 'm', 0,
    0x00, 0x01, 0x00, 0x01,
};

int main(void) {
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    check(s >= 0, "socket(AF_INET, SOCK_DGRAM)");
    errno = 0;
    check(socket(AF_INET, SOCK_RAW, 0) < 0 && errno == ESOCKTNOSUPPORT, "SOCK_RAW is not supported (ESOCKTNOSUPPORT)");
    errno = 0;
    check(socket(AF_INET, SOCK_DGRAM, IPPROTO_TCP) < 0 && errno == EPROTONOSUPPORT, "a datagram socket with protocol TCP -> EPROTONOSUPPORT");

    char buf[512];
    errno = 0;
    check(recvfrom(s, buf, sizeof buf, MSG_DONTWAIT, 0, 0) < 0 && errno == EAGAIN, "recv on an empty socket with MSG_DONTWAIT -> EAGAIN");

    struct sockaddr_in any = addr_of("0.0.0.0", 0);
    check(bind(s, (struct sockaddr *)&any, sizeof any) == 0, "bind(0.0.0.0:0) picks a port");
    struct sockaddr_in me;
    socklen_t len = sizeof me;
    check(getsockname(s, (struct sockaddr *)&me, &len) == 0 && me.sin_family == AF_INET && ntohs(me.sin_port) >= 49152 && len == sizeof me,
          "getsockname reports AF_INET and an ephemeral port");

    int t = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in same = addr_of("0.0.0.0", ntohs(me.sin_port));
    errno = 0;
    check(bind(t, (struct sockaddr *)&same, sizeof same) < 0 && errno == EADDRINUSE, "binding a taken port -> EADDRINUSE");
    close(t);

    int nb = socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0);
    errno = 0;
    check(recv(nb, buf, sizeof buf, 0) < 0 && errno == EAGAIN, "SOCK_NONBLOCK recv -> EAGAIN");
    close(nb);

    /* DHCP runs in the background from boot: wait for an address. */
    struct sockaddr_in dns = addr_of("10.0.2.3", 53);
    ssize_t sent = -1;
    for (int i = 0; i < 100 && sent < 0; i++) {
        sent = sendto(s, QUERY, sizeof QUERY, 0, (struct sockaddr *)&dns, sizeof dns);
        if (sent < 0) usleep(100 * 1000);
    }
    check(sent == (ssize_t)sizeof QUERY, "sendto the DNS server (waits for the DHCP lease)");

    struct pollfd p = { .fd = s, .events = POLLIN };
    int r = poll(&p, 1, 8000);
    check(r == 1 && (p.revents & POLLIN), "poll reports the reply readable");

    struct sockaddr_in from;
    len = sizeof from;
    ssize_t n = recvfrom(s, buf, sizeof buf, 0, (struct sockaddr *)&from, &len);
    check(n >= 12 && (unsigned char)buf[0] == 0x42 && (unsigned char)buf[1] == 0x42 && (buf[2] & 0x80), "DNS response carries our id and QR bit");
    check(n > 0 && from.sin_addr.s_addr == dns.sin_addr.s_addr && ntohs(from.sin_port) == 53, "recvfrom reports the server as the sender");

    /* connect() makes send/write/recv work and getpeername answer. */
    check(connect(s, (struct sockaddr *)&dns, sizeof dns) == 0, "connect() to the resolver");
    struct sockaddr_in peer;
    len = sizeof peer;
    check(getpeername(s, (struct sockaddr *)&peer, &len) == 0 && peer.sin_addr.s_addr == dns.sin_addr.s_addr, "getpeername returns it");
    check(write(s, QUERY, sizeof QUERY) == (ssize_t)sizeof QUERY, "write() on a connected socket");
    n = recv(s, buf, sizeof buf, 0); /* blocks until the answer arrives */
    check(n >= 12 && (unsigned char)buf[0] == 0x42, "blocking recv() gets the second answer");

    close(s);
    printf(failures ? "udp_test: FAIL (%d)\n" : "udp_test: PASS\n", failures);
    return failures != 0;
}
