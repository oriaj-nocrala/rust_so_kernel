// Capability rights on descriptors (Capsicum's model): the kernel side is vfs/src/rights.rs, whose bit layout this
// mirrors (keep them equal). Linux has no Capsicum calls; these numbers are this kernel's own (docs/reference/syscalls.md).
#ifndef CONSTANOS_CAPSICUM_H
#define CONSTANOS_CAPSICUM_H

#include <stdint.h>
#include <errno.h>

typedef uint64_t cap_rights_t;

#define CAP_READ            (1ull << 0)
#define CAP_WRITE           (1ull << 1)
#define CAP_SEEK            (1ull << 2)
#define CAP_MMAP            (1ull << 3)
#define CAP_FSTAT           (1ull << 4)
#define CAP_FTRUNCATE       (1ull << 5)
#define CAP_FCHMOD          (1ull << 6)
#define CAP_FUTIMES         (1ull << 7)
#define CAP_IOCTL           (1ull << 8)
#define CAP_FCNTL           (1ull << 9)
#define CAP_EVENT           (1ull << 10)
#define CAP_LOOKUP          (1ull << 11)
#define CAP_FCHDIR          (1ull << 12)
#define CAP_CREATE          (1ull << 13)
#define CAP_MKDIRAT         (1ull << 14)
#define CAP_SYMLINKAT       (1ull << 15)
#define CAP_UNLINKAT        (1ull << 16)
#define CAP_RENAMEAT_SOURCE (1ull << 17)
#define CAP_RENAMEAT_TARGET (1ull << 18)
#define CAP_LINKAT_SOURCE   (1ull << 19)
#define CAP_LINKAT_TARGET   (1ull << 20)
#define CAP_ACCEPT          (1ull << 21)
#define CAP_CONNECT         (1ull << 22)
#define CAP_BIND            (1ull << 23)
#define CAP_LISTEN          (1ull << 24)
#define CAP_SHUTDOWN        (1ull << 25)
#define CAP_GETPEERNAME     (1ull << 26)
#define CAP_GETSOCKNAME     (1ull << 27)
#define CAP_GETSOCKOPT      (1ull << 28)
#define CAP_SETSOCKOPT      (1ull << 29)
#define CAP_PDKILL          (1ull << 30)
#define CAP_ALL             ((1ull << 31) - 1)

// Errors (outside Linux's range, which ends at 133).
#define ENOTCAPABLE 134
#define ECAPMODE    135

#define SYS_cap_rights_limit 405
#define SYS_cap_rights_get   406

static inline long constanos_cap_syscall2(long nr, long a, long b) {
    long ret;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(nr), "D"(a), "S"(b) : "rcx", "r11", "memory");
    return ret;
}

// Narrow `fd`'s rights to `rights` (never widens). 0 or -1 with errno, like libc.
static inline int cap_rights_limit(int fd, cap_rights_t rights) {
    long r = constanos_cap_syscall2(SYS_cap_rights_limit, fd, (long)rights);
    if (r < 0) { errno = (int)-r; return -1; }
    return 0;
}

static inline int cap_rights_get(int fd, cap_rights_t *rights) {
    long r = constanos_cap_syscall2(SYS_cap_rights_get, fd, (long)rights);
    if (r < 0) { errno = (int)-r; return -1; }
    return 0;
}

#endif
