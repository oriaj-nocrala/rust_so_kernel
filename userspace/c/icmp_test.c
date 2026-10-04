// userspace/c/icmp_test.c
//
// AF_INET/SOCK_RAW/IPPROTO_ICMP through mlibc, the way ping uses it: the
// caller builds an ICMP echo request, the kernel adds the IPv4 header, and
// what comes back is a whole IP packet. Pings QEMU's gateway (10.0.2.2).
// Prints one line per check, then PASS/FAIL.

#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static int failures = 0;

static void check(int ok, const char *what) {
    printf("%s %s\n", ok ? "  ok  " : "  FAIL", what);
    if (!ok) failures++;
}

static uint16_t checksum(const void *data, size_t len) {
    const uint8_t *p = data;
    uint32_t sum = 0;
    for (; len > 1; len -= 2, p += 2) sum += (uint32_t)(p[0] << 8 | p[1]);
    if (len) sum += (uint32_t)(p[0] << 8);
    while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
    return (uint16_t)~sum;
}

int main(void) {
    int s = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
    if (s < 0) printf("socket failed: errno %d\n", errno);
    check(s >= 0, "socket(AF_INET, SOCK_RAW, IPPROTO_ICMP)");
    errno = 0;
    check(socket(AF_INET, SOCK_RAW, IPPROTO_UDP) < 0 && errno == EPROTONOSUPPORT, "a raw socket for another protocol -> EPROTONOSUPPORT");
    int ty = 0;
    socklen_t tl = sizeof ty;
    check(getsockopt(s, SOL_SOCKET, SO_TYPE, &ty, &tl) == 0 && ty == SOCK_RAW, "SO_TYPE is SOCK_RAW");
    int ttl = 64;
    check(setsockopt(s, IPPROTO_IP, IP_TTL, &ttl, sizeof ttl) == 0, "setsockopt(IP_TTL) is accepted");

    uint8_t pkt[8 + 16];
    memset(pkt, 0, sizeof pkt);
    pkt[0] = 8; /* echo request */
    pkt[4] = 0x13; pkt[5] = 0x37; /* id */
    pkt[6] = 0; pkt[7] = 5;       /* seq */
    memcpy(pkt + 8, "constanos-ping..", 16);
    uint16_t ck = checksum(pkt, sizeof pkt);
    pkt[2] = ck >> 8; pkt[3] = ck & 0xff;

    struct sockaddr_in gw;
    memset(&gw, 0, sizeof gw);
    gw.sin_family = AF_INET;
    inet_pton(AF_INET, "10.0.2.2", &gw.sin_addr);

    /* Wait for the DHCP lease, then ping (the first packet may be lost to ARP: resend). */
    ssize_t n = -1;
    uint8_t buf[256];
    struct sockaddr_in from;
    socklen_t fl = sizeof from;
    for (int i = 0; i < 100 && n < 0; i++) {
        if (sendto(s, pkt, sizeof pkt, 0, (struct sockaddr *)&gw, sizeof gw) != (ssize_t)sizeof pkt) {
            usleep(100 * 1000);
            continue;
        }
        struct pollfd p = { .fd = s, .events = POLLIN };
        if (poll(&p, 1, 300) == 1) {
            fl = sizeof from;
            n = recvfrom(s, buf, sizeof buf, 0, (struct sockaddr *)&from, &fl);
        }
    }
    check(n > 20 + 8, "an echo reply arrives (after the DHCP lease)");
    int ihl = n > 0 ? (buf[0] & 0x0f) * 4 : 0;
    check(n > 0 && (buf[0] >> 4) == 4 && ihl == 20 && buf[9] == IPPROTO_ICMP, "it is a whole IPv4 packet carrying ICMP");
    check(n > 0 && from.sin_addr.s_addr == gw.sin_addr.s_addr, "recvfrom reports the gateway as the sender");
    check(n > 0 && checksum(buf, ihl) == 0, "the IP header checksum verifies");
    check(n > 28 && buf[ihl] == 0 && buf[ihl + 4] == 0x13 && buf[ihl + 5] == 0x37 && buf[ihl + 7] == 5, "echo reply (type 0) with our id and sequence");
    check(n >= ihl + 8 + 16 && memcmp(buf + ihl + 8, "constanos-ping..", 16) == 0 && checksum(buf + ihl, n - ihl) == 0, "payload intact, ICMP checksum verifies");

    close(s);
    printf(failures ? "icmp_test: FAIL (%d)\n" : "icmp_test: PASS\n", failures);
    return failures != 0;
}
