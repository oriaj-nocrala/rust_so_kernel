#ifndef _ABIBITS_STATFS_H
#define _ABIBITS_STATFS_H

// Upstream mlibc gates this whole header behind __MLIBC_LINUX_OPTION
// ("statfs() is inherently Linux specific, enable the Linux option or
// don't use this header") — this port deliberately does the latter: pulls
// in just this one struct (via sys/statfs.h, itself copied from mlibc's
// disabled "linux" option) without the rest of that option's machinery.
// The struct layout below has no actual Linux-kernel dependency, just a
// name upstream associated with one.
#include <mlibc-config.h>

#include <abi-bits/fsblkcnt_t.h>
#include <abi-bits/fsfilcnt_t.h>

typedef struct __mlibc_fsid {
	int __val[2];
} fsid_t;

/* WARNING: keep `statfs` and `statfs64` in sync or bad things will happen! */
struct statfs {
	unsigned long f_type;
	unsigned long f_bsize;
	fsblkcnt_t f_blocks;
	fsblkcnt_t f_bfree;
	fsblkcnt_t f_bavail;
	fsfilcnt_t f_files;
	fsfilcnt_t f_ffree;
	fsid_t f_fsid;
	unsigned long f_namelen;
	unsigned long f_frsize;
	unsigned long f_flags;
	unsigned long __f_spare[4];
};

/* WARNING: keep `statfs` and `statfs64` in sync or bad things will happen! */
struct statfs64 {
	unsigned long f_type;
	unsigned long f_bsize;
	fsblkcnt_t f_blocks;
	fsblkcnt_t f_bfree;
	fsblkcnt_t f_bavail;
	fsfilcnt_t f_files;
	fsfilcnt_t f_ffree;
	fsid_t f_fsid;
	unsigned long f_namelen;
	unsigned long f_frsize;
	unsigned long f_flags;
	unsigned long __f_spare[4];
};

#endif /* _ABIBITS_STATFS_H */

