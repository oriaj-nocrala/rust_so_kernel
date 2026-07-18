#ifndef _SYS_STATFS_H
#define _SYS_STATFS_H

// Copied from mlibc/options/linux/include/sys/statfs.h — that option is
// disabled for this port (sysdep_supported_options.linux = false in
// meson.build, to avoid pulling in the rest of the Linux-specific option
// set), but this one thin wrapper around abi-bits/statfs.h (which this
// port already installs) is all BusyBox's libbb.h actually needs.

#ifdef __cplusplus
extern "C" {
#endif

#include <abi-bits/statfs.h>

#ifndef __MLIBC_ABI_ONLY

int statfs(const char *__path, struct statfs *__buf);
int fstatfs(int __fd, struct statfs *__buf);
int fstatfs64(int __fd, struct statfs64 *__buf);

#endif /* !__MLIBC_ABI_ONLY */

#ifdef __cplusplus
}
#endif

#endif /* _SYS_STATFS_H */
