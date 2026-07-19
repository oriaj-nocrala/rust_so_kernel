#ifndef _SYS_SYSINFO_H
#define _SYS_SYSINFO_H

// Standalone port of mlibc/options/linux/include/sys/sysinfo.h — that
// option is disabled for this port (sysdep_supported_options.linux =
// false in meson.build), but BusyBox `free` calls sysinfo() unconditionally
// (procps/free.c has no /proc/meminfo-only fallback), so this thin header
// plus a real sysinfo() implementation in generic.cpp (not gated behind
// mlibc's Linux-option sysdep-hook machinery — this port just defines the
// public function directly) is cheaper than pulling in the whole Linux
// option set for one struct. Layout matches the stable glibc/musl ABI.

#ifdef __cplusplus
extern "C" {
#endif

struct sysinfo {
	long uptime;
	unsigned long loads[3];
	unsigned long totalram;
	unsigned long freeram;
	unsigned long sharedram;
	unsigned long bufferram;
	unsigned long totalswap;
	unsigned long freeswap;
	unsigned short procs;
	unsigned short pad;
	unsigned long totalhigh;
	unsigned long freehigh;
	unsigned int mem_unit;
	char _f[20 - 2 * sizeof(long) - sizeof(int)];
};

int sysinfo(struct sysinfo *__info);

#ifdef __cplusplus
}
#endif

#endif /* _SYS_SYSINFO_H */
