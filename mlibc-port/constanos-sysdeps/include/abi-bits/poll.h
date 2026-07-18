#ifndef _ABIBITS_POLL_H
#define _ABIBITS_POLL_H

// Values must match kernel/src/process/syscall.rs's POLLIN/POLLOUT
// constants exactly — `events`/`revents` cross the syscall boundary as a
// raw bitmask the kernel compares directly, no translation either way.
// The kernel only ever sets POLLIN/POLLOUT; the rest are defined here at
// their standard Linux bit positions for source compatibility, but nothing
// on the kernel side will ever produce them.
#define POLLIN 0x0001
#define POLLPRI 0x0002
#define POLLOUT 0x0004
#define POLLERR 0x0008
#define POLLHUP 0x0010
#define POLLNVAL 0x0020
#define POLLRDNORM 0x0040
#define POLLRDBAND 0x0080
#define POLLWRNORM 0x0100
#define POLLWRBAND 0x0200
#define POLLMSG 0x0400
#define POLLRDHUP 0x2000

#endif /* _ABIBITS_POLL_H */
