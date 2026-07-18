#ifndef _SYS_SYSMACROS_H
#define _SYS_SYSMACROS_H

// glibc-compatible major()/minor()/makedev() — pulled in unconditionally
// by anything that checks `#if !defined(major) || defined(__GLIBC__)`
// (e.g. BusyBox's include/libbb.h), since this port enables mlibc's glibc
// compat option (sysdep_supported_options.glibc = true in meson.build).
//
// This kernel's device numbers (Stat::st_rdev, see kernel/src/fs/types.rs)
// are always 0 — there's no real major/minor encoding anywhere yet — so
// these only need to exist and compile, not encode anything meaningful.
// Bit layout matches glibc's for whenever that changes.

#ifdef __cplusplus
extern "C" {
#endif

static __inline unsigned int gnu_dev_major(unsigned long long int dev) {
	return ((dev >> 8) & 0xfff) | ((unsigned int)(dev >> 32) & ~0xfff);
}

static __inline unsigned int gnu_dev_minor(unsigned long long int dev) {
	return (dev & 0xff) | ((unsigned int)(dev >> 12) & ~0xff);
}

static __inline unsigned long long int gnu_dev_makedev(unsigned int major, unsigned int minor) {
	return ((minor & 0xff) | ((major & 0xfff) << 8)
		| (((unsigned long long int)(minor & ~0xff)) << 12)
		| (((unsigned long long int)(major & ~0xfff)) << 32));
}

#ifdef __cplusplus
}
#endif

#define major(dev) gnu_dev_major(dev)
#define minor(dev) gnu_dev_minor(dev)
#define makedev(maj, min) gnu_dev_makedev(maj, min)

#endif /* _SYS_SYSMACROS_H */
