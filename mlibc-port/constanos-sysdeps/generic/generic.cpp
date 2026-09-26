// mlibc sysdeps port for ConstanOS.
//
// This kernel's syscall ABI (kernel/src/process/syscall.rs) uses a single
// return register (rax): a negative value means -errno, a non-negative
// value is the success result. This differs from dripos-style ports (which
// this file was originally modeled on) that use a dual rax/rdx convention —
// all raw_syscall() call sites below were adapted accordingly.
//
// Entered via the `syscall` instruction: rax=nr, rdi/rsi/rdx/r10/r8=args,
// rcx/r11 clobbered by the instruction itself (identical to Linux x86-64).

#include <bits/ensure.h>
#include <mlibc/debug.hpp>
#include <mlibc/all-sysdeps.hpp>
#include <mlibc/fsfd_target.hpp>
#include <mlibc/thread-entry.hpp>
#include <mlibc/tcb.hpp>
#include <errno.h>
#include <dirent.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <mntent.h>
#include <sys/mman.h>
#include <sched.h>
#include <sys/resource.h>
#include <sys/times.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/sysinfo.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/utsname.h>
#include <termios.h>
#include <unistd.h>

namespace {

// Six argument registers, not five: sendto(44)/recvfrom(45) are the first
// syscalls in this port that use the full SysV syscall register set
// (rdi, rsi, rdx, r10, r8, r9).
inline long raw_syscall(long nr, long a1 = 0, long a2 = 0, long a3 = 0,
                         long a4 = 0, long a5 = 0, long a6 = 0) {
	long ret;
	register long r10 asm("r10") = a4;
	register long r8  asm("r8")  = a5;
	register long r9  asm("r9")  = a6;
	asm volatile ("syscall"
			: "=a"(ret)
			: "a"(nr), "D"(a1), "S"(a2), "d"(a3), "r"(r10), "r"(r8), "r"(r9)
			: "rcx", "r11", "memory");
	return ret;
}

// Syscall numbers — must match kernel/src/process/syscall.rs::SyscallNumber.
constexpr long SYS_read = 0;
constexpr long SYS_write = 1;
constexpr long SYS_open = 2;
constexpr long SYS_close = 3;
constexpr long SYS_stat = 4;
constexpr long SYS_fstat = 5;
constexpr long SYS_getdents64 = 217;
constexpr long SYS_sigaction = 13;
constexpr long SYS_sigprocmask = 14;
constexpr long SYS_rt_sigsuspend = 130;
constexpr long SYS_pause = 34;
// SYS_sigreturn(15) is never called directly by userspace — only the
// kernel-mapped trampoline page uses it (see kernel/src/memory/
// signal_trampoline.rs); mlibc's sigaction() doesn't need to know it
// exists, since this kernel injects the trampoline transparently instead
// of relying on a userspace-supplied sa_restorer.
constexpr long SYS_poll = 7;
constexpr long SYS_lseek = 8;
constexpr long SYS_mmap = 9;
constexpr long SYS_sched_yield = 24;
constexpr long SYS_ftruncate = 77;
constexpr long SYS_memfd_create = 319;
constexpr long SYS_getcwd = 79;
constexpr long SYS_chdir = 80;
constexpr long SYS_rename = 82;
constexpr long SYS_mkdir = 83;
constexpr long SYS_rmdir = 84;
constexpr long SYS_unlink = 87;
constexpr long SYS_lstat = 6;
constexpr long SYS_readlink = 89;
constexpr long SYS_access = 21;
constexpr long SYS_symlink = 88;
constexpr long SYS_chmod = 90;
constexpr long SYS_fchmod = 91;
constexpr long SYS_socket = 41;
constexpr long SYS_connect = 42;
constexpr long SYS_accept = 43;
constexpr long SYS_sendto = 44;
constexpr long SYS_recvfrom = 45;
constexpr long SYS_sendmsg = 46;
constexpr long SYS_recvmsg = 47;
constexpr long SYS_shutdown = 48;
constexpr long SYS_bind = 49;
constexpr long SYS_listen = 50;
constexpr long SYS_getsockname = 51;
constexpr long SYS_getpeername = 52;
constexpr long SYS_socketpair = 53;
constexpr long SYS_setsockopt = 54;
constexpr long SYS_getsockopt = 55;
constexpr long SYS_accept4 = 288;
constexpr long SYS_statvfs = 404;
constexpr long SYS_uptime_sec = 401;
constexpr long SYS_dup = 32;
constexpr long SYS_dup2 = 33;
constexpr long SYS_fcntl = 72;
constexpr long SYS_pipe = 22;
constexpr long SYS_munmap = 11;
constexpr long SYS_ioctl = 16;
constexpr long SYS_nanosleep = 35;
constexpr long SYS_getpid = 39;
constexpr long SYS_getppid = 110;
constexpr long SYS_clone = 56;
constexpr long SYS_fork = 57;
constexpr long SYS_execve = 59;
constexpr long SYS_exit = 60;
constexpr long SYS_waitpid = 61;
constexpr long SYS_kill = 62;
constexpr long SYS_setpgid = 109;
constexpr long SYS_setsid = 112;
constexpr long SYS_getpgid = 121;
constexpr long SYS_getsid = 124;
constexpr long SYS_arch_prctl = 158;

// Not real syscall numbers — internal ioctl `request` values this port
// passes through `SYS_ioctl` for tcgetattr/tcsetattr (see `sys_tcgetattr`/
// `sys_tcsetattr` below), same convention real glibc uses on Linux.
constexpr long TCGETS_REQ = 0x5401;
constexpr long TCSETS_REQ = 0x5402;
constexpr long TCSETSW_REQ = 0x5403;
constexpr long TCSETSF_REQ = 0x5404;
constexpr long SYS_futex = 202;
constexpr long SYS_clock_gettime = 228;
constexpr long SYS_clock_getres = 229;
constexpr long SYS_times = 100;
constexpr long SYS_getrusage = 98;
constexpr long SYS_sysinfo = 99;
constexpr long SYS_sched_getaffinity = 204;
constexpr long SYS_utimensat = 280;

constexpr long ARCH_SET_FS = 0x1002;
constexpr long FUTEX_WAIT = 0;
constexpr long FUTEX_WAKE = 1;

} // namespace

// Static-linking-only substitute for the dynamic linker's per-module handle.
// We never load shared objects, so a single dummy definition is sufficient
// to satisfy __cxa_atexit() call sites pulled in by static C++ initializers
// (e.g. stdio's buffering globals).
extern "C" void *__dso_handle = (void *)&__dso_handle;

// What crtbegin.o's __do_global_dtors_aux does, and what mlibc's static
// build expects from it (options/lsb/generic/dso_exit.cpp: "in static
// builds, these should be provided by the crtbegin.o/crtend.o"). We link
// with -nostdlib and only crt1.o + libc.a, so nothing ever ran the C++
// global destructors registered through __cxa_atexit(..., &__dso_handle) —
// among them mlibc's stdio_guard, which flushes every FILE at exit. On a
// terminal stdout is line-buffered and nothing was lost; to a file or a
// pipe it is fully buffered, and `hello > f` or `fpu_test | wc -l` got 0
// bytes. exit() runs this through __dlapi_exit's .fini_array pass, after
// the plain atexit() handlers, as with crtbegin.o.
extern "C" void __cxa_finalize(void *dso);
[[gnu::destructor]] static void constanos_run_global_dtors() {
	__cxa_finalize(&__dso_handle);
}

namespace mlibc {

void sys_libc_log(const char *message) {
	size_t len = __builtin_strlen(message);
	raw_syscall(SYS_write, 2, (long)message, (long)len);
}

void sys_libc_panic() {
	mlibc::infoLogger() << "\e[31mmlibc: panic!" << frg::endlog;
	raw_syscall(SYS_exit, 1);
	__builtin_trap();
}

int sys_tcb_set(void *pointer) {
	long ret = raw_syscall(SYS_arch_prctl, ARCH_SET_FS, (long)pointer);
	return ret < 0 ? (int)-ret : 0;
}

int sys_anon_allocate(size_t size, void **pointer) {
	long ret = raw_syscall(SYS_mmap, 0, (long)size, PROT_READ | PROT_WRITE,
			MAP_ANONYMOUS, -1);
	if (ret < 0)
		return (int)-ret;
	*pointer = (void *)ret;
	return 0;
}

int sys_anon_free(void *pointer, size_t size) {
	long ret = raw_syscall(SYS_munmap, (long)pointer, (long)size);
	return ret < 0 ? (int)-ret : 0;
}

#ifndef MLIBC_BUILDING_RTLD
void sys_exit(int status) {
	raw_syscall(SYS_exit, status);
	__builtin_trap();
}
#endif

#ifndef MLIBC_BUILDING_RTLD
int sys_clock_get(int clock, time_t *secs, long *nanos) {
	long ts[2] = {0, 0};
	long ret = raw_syscall(SYS_clock_gettime, clock, (long)ts);
	if (ret < 0)
		return (int)-ret;
	*secs = (time_t)ts[0];
	*nanos = ts[1];
	return 0;
}
#endif

#ifndef MLIBC_BUILDING_RTLD
int sys_clock_getres(int clock, time_t *secs, long *nanos) {
	long ts[2] = {0, 0};
	long ret = raw_syscall(SYS_clock_getres, clock, (long)ts);
	if (ret < 0)
		return (int)-ret;
	*secs = (time_t)ts[0];
	*nanos = ts[1];
	return 0;
}

// struct tms is four clock_t (long) — the kernel's layout exactly.
int sys_times(struct tms *tms, clock_t *out) {
	long ret = raw_syscall(SYS_times, (long)tms);
	if (ret < 0)
		return (int)-ret;
	*out = (clock_t)ret;
	return 0;
}

// Linux's utimensat(2) as is; a NULL pathname is futimens(dirfd).
int sys_utimensat(int dirfd, const char *pathname, const struct timespec times[2], int flags) {
	long ret = raw_syscall(SYS_utimensat, dirfd, (long)pathname, (long)times, flags);
	return ret < 0 ? (int)-ret : 0;
}

// Only ru_utime/ru_stime are filled by the kernel; it zeroes the rest.
int sys_getrusage(int scope, struct rusage *usage) {
	long ret = raw_syscall(SYS_getrusage, scope, (long)usage);
	return ret < 0 ? (int)-ret : 0;
}

namespace {

// CPUs the kernel will run this process on (sched_getaffinity), or 1 if it
// cannot say.
long online_cpus() {
	unsigned long mask[16] = {};
	long ret = raw_syscall(SYS_sched_getaffinity, 0, (long)sizeof(mask), (long)mask);
	if (ret <= 0)
		return 1;
	long n = 0;
	for (long i = 0; i < ret / (long)sizeof(unsigned long); i++)
		n += __builtin_popcountl(mask[i]);
	return n > 0 ? n : 1;
}

} // namespace

// What mlibc's generic sysconf() would otherwise answer with a red warning
// and a made-up number: _SC_CLK_TCK was 1000000 (so `ps` TIME and every
// times()-based measurement were off by 10^4) and both processor counts
// were 1. EINVAL hands everything else back to mlibc's defaults.
int sys_sysconf(int num, long *ret) {
	switch (num) {
	case _SC_CLK_TCK:
		*ret = 100; // the kernel's tick, sched::cputime::USER_HZ
		return 0;
	case _SC_NPROCESSORS_ONLN:
	case _SC_NPROCESSORS_CONF:
		// CONF counts CPUs that could come online; every CPU this kernel
		// found either runs processes or never will, so both agree.
		*ret = online_cpus();
		return 0;
	case _SC_OPEN_MAX:
		*ret = 16; // FileDescriptorTable's MAX_FILES
		return 0;
	case _SC_PHYS_PAGES:
	case _SC_AVPHYS_PAGES: {
		struct statvfs sv{};
		if (raw_syscall(SYS_statvfs, (long)"/", (long)&sv) < 0)
			return EINVAL;
		unsigned long blocks = num == _SC_PHYS_PAGES ? sv.f_blocks : sv.f_bfree;
		*ret = (long)(blocks * sv.f_bsize / 4096);
		return 0;
	}
	default:
		return EINVAL;
	}
}
#endif

int sys_open(const char *path, int flags, mode_t, int *fd) {
	long ret = raw_syscall(SYS_open, (long)path, flags);
	if (ret < 0)
		return (int)-ret;
	*fd = (int)ret;
	return 0;
}

int sys_close(int fd) {
	long ret = raw_syscall(SYS_close, fd);
	return ret < 0 ? (int)-ret : 0;
}

// `flags` here is always 0 from mlibc's dup()/dup2() frontends (it'd only
// be nonzero for a dup3()-style O_CLOEXEC-on-the-new-fd request, which
// nothing in this port calls) — this kernel's dup/dup2 syscalls don't take
// a flags argument at all, so it's simply dropped rather than threaded
// through for no observable effect.
int sys_dup(int fd, int flags, int *newfd) {
	(void)flags;
	long ret = raw_syscall(SYS_dup, fd);
	if (ret < 0)
		return (int)-ret;
	*newfd = (int)ret;
	return 0;
}

int sys_dup2(int fd, int flags, int newfd) {
	(void)flags;
	long ret = raw_syscall(SYS_dup2, fd, newfd);
	return ret < 0 ? (int)-ret : 0;
}

// This kernel's fcntl(72) only really implements F_DUPFD/F_DUPFD_CLOEXEC
// (both dup(), since there's no per-fd CLOEXEC flag to set differently)
// and stubs F_GETFD/F_SETFD/F_GETFL/F_SETFL (fd-validity-checked, but no
// real flags storage backs them — see the kernel-side doc comment). Only
// F_DUPFD/F_DUPFD_CLOEXEC/F_SETFD/F_SETFL take a variadic argument; the
// getters don't, so `va_arg` is only pulled for the ones that need it.
int sys_fcntl(int fd, int request, va_list args, int *result) {
	// All commands this kernel understands take an `int`-sized argument
	// (POSIX: F_DUPFD's is "the minimum new fd", F_SETFD/F_SETFL's are
	// flag bits) — none need `long`/pointer-sized varargs.
	long arg = 0;
	switch (request) {
		case F_DUPFD:
		case F_DUPFD_CLOEXEC:
		case F_SETFD:
		case F_SETFL:
			arg = va_arg(args, int);
			break;
		default:
			break;
	}
	long ret = raw_syscall(SYS_fcntl, fd, request, arg);
	if (ret < 0)
		return (int)-ret;
	*result = (int)ret;
	return 0;
}

namespace {

// Mirrors kernel/src/fs/types.rs::Stat exactly (144 bytes, field order and
// all) — this is NOT the same layout as mlibc's own `struct stat` (see
// include/abi-bits/stat.h: field order differs and it packs times into
// `struct timespec` triplets instead of separate sec/nsec pairs), so a
// syscall result has to land here first and get field-by-field converted
// below rather than being read directly into the caller's `struct stat *`.
struct KernelStat {
	uint64_t st_dev;
	uint64_t st_ino;
	uint64_t st_nlink;
	uint32_t st_mode;
	uint32_t st_uid;
	uint32_t st_gid;
	uint32_t _pad0;
	uint64_t st_rdev;
	int64_t  st_size;
	int64_t  st_blksize;
	int64_t  st_blocks;
	// Named *_sec (not st_atime/st_mtime/st_ctime) because <sys/stat.h>
	// #defines those exact identifiers as legacy `st_atim.tv_sec`-style
	// macros for BSD source compatibility — using them here as plain field
	// names would silently rewrite this struct's declaration via macro
	// substitution and fail to compile.
	uint64_t st_atime_sec;
	uint64_t st_atime_nsec;
	uint64_t st_mtime_sec;
	uint64_t st_mtime_nsec;
	uint64_t st_ctime_sec;
	uint64_t st_ctime_nsec;
	int64_t  _reserved[3];
};
static_assert(sizeof(KernelStat) == 144, "KernelStat must match kernel's Stat layout");

void convert_stat(const KernelStat &in, struct stat *out) {
	out->st_dev = in.st_dev;
	out->st_ino = in.st_ino;
	out->st_mode = in.st_mode;
	out->st_nlink = in.st_nlink;
	out->st_uid = in.st_uid;
	out->st_gid = in.st_gid;
	out->st_rdev = in.st_rdev;
	out->st_size = in.st_size;
	out->st_atim.tv_sec = (time_t)in.st_atime_sec;
	out->st_atim.tv_nsec = (long)in.st_atime_nsec;
	out->st_mtim.tv_sec = (time_t)in.st_mtime_sec;
	out->st_mtim.tv_nsec = (long)in.st_mtime_nsec;
	out->st_ctim.tv_sec = (time_t)in.st_ctime_sec;
	out->st_ctim.tv_nsec = (long)in.st_ctime_nsec;
	out->st_blksize = in.st_blksize;
	out->st_blocks = in.st_blocks;
}

} // namespace

// Backs stat()/lstat()/fstat()/fstatat() alike (mlibc funnels all four
// through this one entry point, discriminated by `fsfdt`). `lstat()` is
// `fsfd_target::path` with `AT_SYMLINK_NOFOLLOW` set in `flags` — real
// symlink support now exists kernel-side (see kernel/src/fs/vfs.rs's
// `resolve`/`resolve_no_follow`), so this now genuinely routes to the
// no-follow SYS_lstat instead of aliasing plain stat(). `fd_path` (fstatat's
// dirfd+relative-path form) is handled the same way sys_unlinkat below
// handles it: this kernel has no dirfd+relative-path syscall, but
// `fstatat(AT_FDCWD, path, ...)` — by far the common case — is just a path
// stat/lstat in disguise, so that degrades to the `path` case; a real dirfd
// has no way to be serviced here.
int sys_stat(fsfd_target fsfdt, int fd, const char *path, int flags,
		struct stat *statbuf) {
	KernelStat ks{};
	long ret;
	switch (fsfdt) {
		case fsfd_target::path:
			ret = raw_syscall((flags & AT_SYMLINK_NOFOLLOW) ? SYS_lstat : SYS_stat,
					(long)path, (long)&ks);
			break;
		case fsfd_target::fd:
			ret = raw_syscall(SYS_fstat, fd, (long)&ks);
			break;
		case fsfd_target::fd_path:
			if (fd != AT_FDCWD)
				return ENOSYS;
			ret = raw_syscall((flags & AT_SYMLINK_NOFOLLOW) ? SYS_lstat : SYS_stat,
					(long)path, (long)&ks);
			break;
		default:
			return EINVAL;
	}
	if (ret < 0)
		return (int)-ret;
	convert_stat(ks, statbuf);
	return 0;
}

// readlink(): this kernel's SYS_readlink(89) returns the byte count written
// (never NUL-terminated, truncated silently if `max_size` is too small) —
// same convention as real Linux, so no post-processing needed beyond
// converting the raw return value into mlibc's (errno, *length) pair.
int sys_readlink(const char *path, void *buffer, size_t max_size, ssize_t *length) {
	long ret = raw_syscall(SYS_readlink, (long)path, (long)buffer, (long)max_size);
	if (ret < 0)
		return (int)-ret;
	*length = (ssize_t)ret;
	return 0;
}

// access(): no per-uid permission model kernel-side, so this is really just
// "does the path resolve, and — for W_OK — is it actually writable" (see
// SYS_access's doc comment in kernel/src/process/syscall.rs for how the
// kernel answers that without a stat-mode-bits permission check). Without
// this hook, mlibc's access() short-circuits to ENOSYS on the weak
// mlibc::sys_access default, which callers that use access() to probe
// writability (e.g. BusyBox `vi` deciding whether to open readonly) treat
// as "assume not writable" — so every file looked permanently readonly.
int sys_access(const char *path, int mode) {
	long ret = raw_syscall(SYS_access, (long)path, mode);
	return ret < 0 ? (int)-ret : 0;
}

// A "directory handle" is just a regular fd here — this kernel's open()
// already returns a readable fd for directories (see DevDirInode/
// InitramfsDirInode's `open()` impls), there's no separate directory-fd
// namespace to allocate.
int sys_open_dir(const char *path, int *handle) {
	long ret = raw_syscall(SYS_open, (long)path, 0);
	if (ret < 0)
		return (int)-ret;
	*handle = (int)ret;
	return 0;
}

// This kernel's getdents64(217) already writes `linux_dirent64`-shaped
// records (see kernel/src/fs/types.rs::DirEntry::write_dirent64: ino(8) +
// off(8) + reclen(2) + type(1) + name), which is byte-for-byte the same
// layout as mlibc's own `struct dirent` (options/posix/include/dirent.h) —
// no per-field conversion needed here, unlike sys_stat above.
int sys_read_entries(int handle, void *buffer, size_t max_size, size_t *bytes_read) {
	long ret = raw_syscall(SYS_getdents64, handle, (long)buffer, (long)max_size);
	if (ret < 0)
		return (int)-ret;
	*bytes_read = (size_t)ret;
	return 0;
}

// This kernel's mkdir(83) has no `mode` parameter (nothing enforces
// permission bits, same reasoning as open()'s missing mode arg) — accept
// and silently drop it, matching mkdir(2)'s signature so callers don't
// need special-casing.
int sys_mkdir(const char *path, mode_t) {
	long ret = raw_syscall(SYS_mkdir, (long)path);
	return ret < 0 ? (int)-ret : 0;
}

int sys_rmdir(const char *path) {
	long ret = raw_syscall(SYS_rmdir, (long)path);
	return ret < 0 ? (int)-ret : 0;
}

int sys_rename(const char *path, const char *new_path) {
	long ret = raw_syscall(SYS_rename, (long)path, (long)new_path);
	return ret < 0 ? (int)-ret : 0;
}

int sys_chdir(const char *path) {
	long ret = raw_syscall(SYS_chdir, (long)path);
	return ret < 0 ? (int)-ret : 0;
}

// The kernel's getcwd(79) matches the real Linux raw-syscall convention
// (unlike this port's usual single-rax-return style, which is already what
// raw_syscall() exposes): returns bytes written (incl. NUL) on success, or
// -errno. mlibc's own getcwd() (options/posix/generic/unistd.cpp) expects
// this sysdeps hook to just return 0/errno, not the byte count — it never
// looks at the count, only whether `buffer` got filled in.
int sys_getcwd(char *buffer, size_t size) {
	long ret = raw_syscall(SYS_getcwd, (long)buffer, (long)size);
	return ret < 0 ? (int)-ret : 0;
}

// mlibc's unlink()/rmdir() call through here (unlinkat(AT_FDCWD, path, 0)
// and, for some callers, unlinkat(AT_FDCWD, path, AT_REMOVEDIR)) rather
// than a plain sys_unlink — there's no `[[gnu::weak]] int sys_unlink(...)`
// declared in posix-sysdeps.hpp at all, only this *at() form. This kernel
// has no directory-fd concept (no openat family), so only AT_FDCWD is
// honored; any other `fd` means the caller wants a syscall relative to an
// open directory descriptor, which doesn't exist here.
int sys_unlinkat(int fd, const char *path, int flags) {
	if (fd != AT_FDCWD)
		return ENOSYS;
	long ret = raw_syscall(flags & AT_REMOVEDIR ? SYS_rmdir : SYS_unlink, (long)path);
	return ret < 0 ? (int)-ret : 0;
}

// This kernel's pipe(22) takes only `int pipefd[2]` — no pipe2() flags
// (O_NONBLOCK/O_CLOEXEC aren't supported). Anything other than 0 in `flags`
// would silently be ignored by the kernel, so reject it here instead.
int sys_pipe(int *fds, int flags) {
	if (flags != 0)
		return EINVAL;
	long ret = raw_syscall(SYS_pipe, (long)fds);
	return ret < 0 ? (int)-ret : 0;
}

int sys_read(int fd, void *buf, size_t count, ssize_t *bytes_read) {
	long ret = raw_syscall(SYS_read, fd, (long)buf, (long)count);
	if (ret < 0)
		return (int)-ret;
	*bytes_read = ret;
	return 0;
}

#ifndef MLIBC_BUILDING_RTLD
int sys_write(int fd, const void *buf, size_t count, ssize_t *bytes_written) {
	long ret = raw_syscall(SYS_write, fd, (long)buf, (long)count);
	if (ret < 0)
		return (int)-ret;
	*bytes_written = ret;
	return 0;
}
#endif

// TCGETS (0x5401): our kernel's sys_ioctl returns 0 for fd 0/1/2 (the
// console), ENOTTY otherwise — exactly the check isatty() needs.
int sys_isatty(int fd) {
	long ret = raw_syscall(SYS_ioctl, fd, TCGETS_REQ, 0);
	return ret < 0 ? (int)-ret : 0;
}

// Generic ioctl() passthrough — backs the public `ioctl()` (glibc option)
// and, via that, `tcgetpgrp`/`tcsetpgrp` (options/posix/generic/unistd.cpp
// calls `ioctl(fd, TIOCGPGRP/TIOCSPGRP, &pgrp)` directly rather than going
// through a dedicated sysdeps hook).
int sys_ioctl(int fd, unsigned long request, void *arg, int *result) {
	long ret = raw_syscall(SYS_ioctl, fd, (long)request, (long)arg);
	if (ret < 0)
		return (int)-ret;
	if (result)
		*result = (int)ret;
	return 0;
}

// mlibc's posix `tcgetattr()`/`tcsetattr()` (options/posix/generic/
// termios.cpp) call these sysdeps hooks directly rather than going through
// `ioctl()` — implemented as thin TCGETS/TCSETS* wrappers around our
// kernel's existing ioctl(16) syscall, the same way real glibc implements
// them. `struct termios` here is this port's own ABI (`abi-bits/termios.h`
// — `cc_t`/`tcflag_t` are `unsigned int`, not the real-POSIX `unsigned
// char`), which is exactly what the kernel's TCGETS/TCSETS* handling
// marshals (see kernel/src/tty.rs::Termios).
int sys_tcgetattr(int fd, struct termios *attr) {
	long ret = raw_syscall(SYS_ioctl, fd, TCGETS_REQ, (long)attr);
	return ret < 0 ? (int)-ret : 0;
}

int sys_tcsetattr(int fd, int opts, const struct termios *attr) {
	long req = TCSETS_REQ;
	if (opts == TCSADRAIN)
		req = TCSETSW_REQ;
	else if (opts == TCSAFLUSH)
		req = TCSETSF_REQ;
	long ret = raw_syscall(SYS_ioctl, fd, req, (long)attr);
	return ret < 0 ? (int)-ret : 0;
}

// No output queue that could be drained or paused exists (a pty's output
// belongs to its master's reader; the console writes straight through), so
// these two are no-ops that report success, same spirit as `sys_brk`.
int sys_tcdrain(int) { return 0; }
int sys_tcflow(int, int) { return 0; }

// TCFLSH (0x540B): a pty really discards (its line discipline, the `tty`
// crate); the console answers 0, having nothing worth discarding.
int sys_tcflush(int fd, int queue) {
	long ret = raw_syscall(SYS_ioctl, fd, 0x540B, queue);
	return ret < 0 ? (int)-ret : 0;
}

// Pseudo-terminals (phase 3.3 of docs/gui/gui-plan.md). mlibc's
// `posix_openpt` opens /dev/ptmx itself and `grantpt` is a no-op; these
// two are the ioctls behind `ptsname` and `unlockpt`.
int sys_ptsname(int fd, char *buffer, size_t length) {
	unsigned int n;
	long ret = raw_syscall(SYS_ioctl, fd, 0x80045430 /* TIOCGPTN */, (long)&n);
	if (ret < 0)
		return (int)-ret;
	if (snprintf(buffer, length, "/dev/pts/%u", n) >= (int)length)
		return ERANGE;
	return 0;
}

int sys_unlockpt(int fd) {
	int unlock = 0;
	long ret = raw_syscall(SYS_ioctl, fd, 0x40045431 /* TIOCSPTLCK */, (long)&unlock);
	return ret < 0 ? (int)-ret : 0;
}

// No /proc/self/fd to read a link from: a pty slave is recognised by the
// inode number the kernel gives it (`ipc::pty::PTS_INO_BASE + n`), and any
// other terminal is the console.
int sys_ttyname(int fd, char *buf, size_t size) {
	if (int e = sys_isatty(fd); e)
		return e;
	struct stat st;
	if (int e = sys_stat(fsfd_target::fd, fd, "", 0, &st); e)
		return e;
	int n;
	if (S_ISCHR(st.st_mode) && st.st_ino >= 200000 && st.st_ino < 200000 + 16)
		n = snprintf(buf, size, "/dev/pts/%u", (unsigned)(st.st_ino - 200000));
	else
		n = snprintf(buf, size, "/dev/console");
	return n >= (int)size ? ERANGE : 0;
}

int sys_setpgid(pid_t pid, pid_t pgid) {
	long ret = raw_syscall(SYS_setpgid, pid, pgid);
	return ret < 0 ? (int)-ret : 0;
}

int sys_getpgid(pid_t pid, pid_t *pgid) {
	long ret = raw_syscall(SYS_getpgid, pid);
	if (ret < 0)
		return (int)-ret;
	*pgid = (pid_t)ret;
	return 0;
}

int sys_getsid(pid_t pid, pid_t *sid) {
	long ret = raw_syscall(SYS_getsid, pid);
	if (ret < 0)
		return (int)-ret;
	*sid = (pid_t)ret;
	return 0;
}

int sys_setsid(pid_t *sid) {
	long ret = raw_syscall(SYS_setsid);
	if (ret < 0)
		return (int)-ret;
	*sid = (pid_t)ret;
	return 0;
}

int sys_seek(int fd, off_t offset, int whence, off_t *new_offset) {
	long ret = raw_syscall(SYS_lseek, fd, offset, whence);
	if (ret < 0)
		return (int)-ret;
	*new_offset = ret;
	return 0;
}

// `struct pollfd { int fd; short events; short revents; }` is already the
// exact layout kernel/src/process/syscall.rs::PollFd uses (8 bytes, no
// padding) — passed straight through, no conversion. This kernel caps
// nfds at 16 and returns EINVAL above that (see sys_poll's `if nfds > 16`
// check); that limit isn't enforced here too, the kernel's own -EINVAL
// return covers it.
int sys_poll(struct pollfd *fds, nfds_t count, int timeout, int *num_events) {
	long ret = raw_syscall(SYS_poll, (long)fds, (long)count, (long)timeout);
	if (ret < 0)
		return (int)-ret;
	*num_events = (int)ret;
	return 0;
}

// select()/pselect() have no syscall of their own in this kernel — only
// poll() (SYS_poll, above) does. Real Linux libc's don't need this: glibc
// implements select() on top of the kernel's own separate select(2), but
// this kernel never grew one (poll() covers the same ground and is what
// epoll/pipe/socket code here already uses) — so mlibc's posix option
// (options/posix/generic/sys-select.cpp) is bridged onto sys_poll instead
// of left unimplemented. ncurses' input-with-timeout path (what drives
// cmatrix's/curses's getch()-with-nodelay animation loop) calls select()
// directly, not poll(), so this was a real gap, not a hypothetical one —
// found via cmatrix flooding the console with "missing sysdep" errors.
//
// timeout/sigmask semantics: sigmask is ignored (this kernel has no
// syscall to atomically swap the signal mask for the duration of a single
// wait, and nothing in this port relies on that race-free guarantee —
// see kill/sigprocmask's own scope). Negative/zero fds and NULL sets are
// handled by simply never marking them ready, matching a real select()'s
// behavior for an unwatched fd.
int sys_pselect(int num_fds, fd_set *read_set, fd_set *write_set, fd_set *except_set,
		const struct timespec *timeout, const sigset_t *, int *num_events) {
	if (num_fds < 0 || num_fds > FD_SETSIZE)
		return EINVAL;

	struct pollfd fds[16];
	if ((size_t)num_fds > sizeof(fds) / sizeof(fds[0]))
		return EINVAL;

	int n = 0;
	for (int fd = 0; fd < num_fds; fd++) {
		short events = 0;
		if (read_set && __FD_ISSET(fd, read_set)) events |= POLLIN;
		if (write_set && __FD_ISSET(fd, write_set)) events |= POLLOUT;
		if (except_set && __FD_ISSET(fd, except_set)) events |= POLLPRI;
		if (!events)
			continue;
		fds[n].fd = fd;
		fds[n].events = events;
		fds[n].revents = 0;
		n++;
	}

	int timeout_ms = -1;
	if (timeout)
		timeout_ms = (int)(timeout->tv_sec * 1000 + timeout->tv_nsec / 1000000);

	int nready = 0;
	int err = sys_poll(fds, n, timeout_ms, &nready);
	if (err)
		return err;

	if (read_set) __FD_ZERO(read_set);
	if (write_set) __FD_ZERO(write_set);
	if (except_set) __FD_ZERO(except_set);

	int count = 0;
	for (int i = 0; i < n; i++) {
		if (!fds[i].revents)
			continue;
		if (read_set && (fds[i].revents & (POLLIN | POLLHUP | POLLERR))) {
			__FD_SET(fds[i].fd, read_set);
			count++;
		}
		if (write_set && (fds[i].revents & (POLLOUT | POLLERR))) {
			__FD_SET(fds[i].fd, write_set);
			count++;
		}
		if (except_set && (fds[i].revents & POLLPRI)) {
			__FD_SET(fds[i].fd, except_set);
			count++;
		}
	}
	*num_events = count;
	return 0;
}

// The kernel takes any nonzero address as MAP_FIXED, so a hint without
// MAP_FIXED is dropped here (Linux treats it as advisory anyway). flags, fd
// and offset go through as they are: MAP_SHARED of a memfd, and
// MAP_SHARED|MAP_ANONYMOUS, are real shared memory (docs/gui/gui-plan.md).
int sys_vm_map(void *hint, size_t size, int prot, int flags,
		int fd, off_t offset, void **window) {
	void *addr = (flags & MAP_FIXED) ? hint : nullptr;
	long ret = raw_syscall(SYS_mmap, (long)addr, (long)size, prot,
			flags, fd, (long)offset);
	if (ret < 0)
		return (int)-ret;
	*window = (void *)ret;
	return 0;
}

int sys_memfd_create(const char *name, int flags, int *fd) {
	long ret = raw_syscall(SYS_memfd_create, (long)name, flags);
	if (ret < 0)
		return (int)-ret;
	*fd = (int)ret;
	return 0;
}

} // namespace mlibc

// mlibc defines memfd_create() only under the Linux option, which this port
// leaves off; setup-mlibc.sh declares it in <sys/mman.h> for every port.
extern "C" int memfd_create(const char *name, unsigned int flags) {
	int fd;
	if (int e = mlibc::sys_memfd_create(name, (int)flags, &fd)) {
		errno = e;
		return -1;
	}
	return fd;
}

namespace mlibc {

void sys_yield() {
	raw_syscall(SYS_sched_yield);
}

int sys_ftruncate(int fd, size_t size) {
	long ret = raw_syscall(SYS_ftruncate, fd, (long)size);
	return ret < 0 ? (int)-ret : 0;
}

int sys_vm_unmap(void *pointer, size_t size) {
	return sys_anon_free(pointer, size);
}

int sys_futex_wait(int *pointer, int expected, const struct timespec *) {
	long ret = raw_syscall(SYS_futex, (long)pointer, FUTEX_WAIT, expected, 0);
	return ret < 0 ? (int)-ret : 0;
}

int sys_futex_wake(int *pointer) {
	long ret = raw_syscall(SYS_futex, (long)pointer, FUTEX_WAKE, 0, 0);
	return ret < 0 ? (int)-ret : 0;
}

// All remaining functions are disabled in ldso.
#ifndef MLIBC_BUILDING_RTLD

// This kernel's clone(56) is a custom ABI, not Linux's real clone(2):
// long clone(void *entry, void *stack, void *tcb). It creates a new
// schedulable thread sharing the caller's AddressSpace, starting execution
// at `entry` with RSP=`stack`; `tcb` is passed through unused by the kernel
// (see kernel/src/process/syscall.rs::sys_clone) — __mlibc_enter_thread
// below sets FS itself via sys_tcb_set() once the new thread actually runs.
// `stack` here is the value thread.cpp's sys_prepare_stack() already built
// (entry/user_arg/tcb pushed on it for __mlibc_start_thread to pop).
int sys_clone(void *tcb, pid_t *tid_out, void *stack) {
	long ret = raw_syscall(SYS_clone, (long)__mlibc_start_thread,
			(long)stack, (long)tcb);
	if (ret < 0)
		return (int)-ret;
	*tid_out = (pid_t)ret;
	return 0;
}

void sys_thread_exit() {
	raw_syscall(SYS_exit, 0);
	__builtin_trap();
}

static long monotonic_ns() {
	long ts[2] = {0, 0};
	raw_syscall(SYS_clock_gettime, 1 /* CLOCK_MONOTONIC */, (long)ts);
	return ts[0] * 1000000000L + ts[1];
}

// The kernel's nanosleep takes plain nanoseconds and reports no remaining
// time, so what is left of an interrupted sleep is measured here — it is
// what sleep() returns. The result used to be discarded: an interrupted
// sleep looked finished (nanosleep 0, sleep() 0) once signals started
// ending sleeps on 2026-09-25.
int sys_sleep(time_t *secs, long *nanos) {
	long ns = (*secs) * 1000000000L + *nanos;
	long t0 = monotonic_ns();
	long ret = raw_syscall(SYS_nanosleep, ns);
	if (ret < 0) {
		long left = ns - (monotonic_ns() - t0);
		if (left < 0)
			left = 0;
		*secs = left / 1000000000L;
		*nanos = left % 1000000000L;
		return (int)-ret;
	}
	*secs = 0;
	*nanos = 0;
	return 0;
}

int sys_fork(pid_t *child) {
	long ret = raw_syscall(SYS_fork);
	if (ret < 0)
		return (int)-ret;
	*child = (pid_t)ret;
	return 0;
}

// This kernel's exec(59) reads argv/envp as plain NULL-terminated arrays
// of C-string pointers straight out of the caller's memory (see
// kernel/src/process/syscall.rs::read_user_str_array) — exactly what
// `char *const argv[]`/`char *const envp[]` already are, so both pass
// through unconverted.
int sys_execve(const char *path, char *const argv[], char *const envp[]) {
	long ret = raw_syscall(SYS_execve, (long)path, (long)argv, (long)envp);
	return ret < 0 ? (int)-ret : 0;
}

// This kernel's waitpid(61) now supports the real POSIX pid overloads
// (`>0` exact pid, `0` own process group, `-1` any child, `<-1` group
// `-pid`) and `flags` (WNOHANG/WUNTRACED — see kernel/src/process/
// syscall.rs::sys_waitpid's doc comment). `pid` and `flags` both pass
// through unconverted; the kernel itself returns ECHILD when nothing
// matches at all, so nothing needs rejecting up front here anymore.
//
// The kernel writes a real status word into a second syscall argument (a
// user pointer) — see kernel/src/process/syscall.rs::sys_waitpid and
// Scheduler::{notify_child_death,notify_child_stopped,resolve_wait_status}
// for how it gets there safely even across the "block, then get woken by
// the child's sys_exit" path (which resumes via a raw trapframe restore
// with no return into Rust code, and originally couldn't write into the
// parent's memory from the dying child's own address space — fixed by
// deferring the write to the next time the parent itself resumes in user
// mode).
int sys_waitpid(pid_t pid, int *status, int flags, struct rusage *ru,
		pid_t *ret_pid) {
	if (ru)
		__builtin_memset(ru, 0, sizeof(*ru));
	int kstatus = 0;
	long ret = raw_syscall(SYS_waitpid, pid, (long)&kstatus, flags);
	if (ret < 0)
		return (int)-ret;
	if (status)
		*status = kstatus;
	if (ret_pid)
		*ret_pid = (pid_t)ret;
	return 0;
}

pid_t sys_getpid() {
	return (pid_t)raw_syscall(SYS_getpid);
}

// It used to return 1 unconditionally, so `kill(getppid(), sig)` from a
// child signalled init instead of its parent (sigsuspend_test's case E).
pid_t sys_getppid() {
	return (pid_t)raw_syscall(SYS_getppid);
}

// No real user/group model exists — this kernel is single-user, everything
// runs as an implicit root (uid==euid==gid==egid==0). ash's startup (and
// anything else calling getuid()/geteuid()) needs these to not be missing
// sysdeps, not for the value to mean anything beyond "not a setuid binary".
uid_t sys_getuid() { return 0; }
uid_t sys_geteuid() { return 0; }
gid_t sys_getgid() { return 0; }
gid_t sys_getegid() { return 0; }

// Single-user kernel, single implicit group (root's, gid 0) — `size == 0`
// is the POSIX "just tell me how many groups there are" probe (must NOT
// touch `list`, since it may be null/undersized on that call).
int sys_getgroups(size_t size, gid_t *list, int *ret) {
	if (size > 0)
		list[0] = 0;
	*ret = 1;
	return 0;
}

// symlink()/symlinkat(): real symlink creation — this kernel's SYS_symlink
// stores `target_path` verbatim under `link_path` (writable filesystems
// only, e.g. ramfs at /tmp; read-only mounts answer EROFS same as
// mkdir/create there). symlinkat(AT_FDCWD, ...) degrades to plain
// symlink(), same trick as sys_fchmodat above.
int sys_symlink(const char *target_path, const char *link_path) {
	long ret = raw_syscall(SYS_symlink, (long)target_path, (long)link_path);
	return ret < 0 ? (int)-ret : 0;
}

int sys_symlinkat(const char *target_path, int dirfd, const char *link_path) {
	if (dirfd != AT_FDCWD)
		return ENOSYS;
	return sys_symlink(target_path, link_path);
}

// chmod()/fchmod(): this kernel has no per-inode permission-bits storage
// (see SYS_chmod's doc comment in kernel/src/process/syscall.rs), so these
// just validate (path exists / fd is open) and otherwise no-op-succeed.
int sys_chmod(const char *path, mode_t mode) {
	long ret = raw_syscall(SYS_chmod, (long)path, mode);
	return ret < 0 ? (int)-ret : 0;
}

int sys_fchmod(int fd, mode_t mode) {
	long ret = raw_syscall(SYS_fchmod, fd, mode);
	return ret < 0 ? (int)-ret : 0;
}

// fchmodat(AT_FDCWD, path, ...) is just chmod(path, ...) in disguise — same
// degrade-to-the-plain-path-syscall trick this port already uses for
// sys_unlinkat/sys_stat's fd_path case, since there's no real dirfd-relative
// syscall to route a non-AT_FDCWD dirfd to.
int sys_fchmodat(int dirfd, const char *path, mode_t mode, int) {
	if (dirfd != AT_FDCWD)
		return ENOSYS;
	return sys_chmod(path, mode);
}

// uname(): fixed, hardcoded fields — this kernel has exactly one "release"
// (whatever's currently running), no kernel-version negotiation concept for
// software to probe.
int sys_uname(struct utsname *out) {
	auto copy = [](char *dst, const char *src) {
		size_t i = 0;
		for (; src[i] && i < 64; i++) dst[i] = src[i];
		dst[i] = '\0';
	};
	copy(out->sysname, "ConstanOS");
	copy(out->nodename, "constanos");
	copy(out->release, "0.1.0");
	copy(out->version, "#1");
	copy(out->machine, "x86_64");
	return 0;
}

// gethostname()/sethostname(): process-local storage (a plain static
// buffer baked into every process's own copy of this translation unit's
// globals) — good enough for `hostname` to report/set something
// consistent within one process's lifetime; a `sethostname` in one shell
// session won't be visible to a process started afterwards, since there's
// no kernel-side global to route it through. Nothing in this kernel's
// current use of `hostname` needs cross-process persistence.
namespace {
char g_hostname[65] = "constanos";
} // namespace

int sys_gethostname(char *buffer, size_t bufsize) {
	size_t len = __builtin_strlen(g_hostname);
	if (len + 1 > bufsize)
		return ENAMETOOLONG;
	__builtin_memcpy(buffer, g_hostname, len + 1);
	return 0;
}

int sys_sethostname(const char *buffer, size_t bufsize) {
	if (bufsize >= sizeof(g_hostname))
		return EINVAL;
	__builtin_memcpy(g_hostname, buffer, bufsize);
	g_hostname[bufsize] = '\0';
	return 0;
}

// statvfs()/fstatvfs(): see SYS_statvfs's doc comment in
// kernel/src/process/syscall.rs — one physical-memory pool backs every
// mount here, so both just report the same live Buddy-allocator-derived
// numbers. fstatvfs has no path to give the kernel (only an fd, and this
// kernel's statvfs doesn't need one beyond "does something resolve here"),
// so it passes a fixed "/" — always resolves, and the kernel-side
// implementation's numbers don't depend on the path anyway.
int sys_statvfs(const char *path, struct statvfs *out) {
	long ret = raw_syscall(SYS_statvfs, (long)path, (long)out);
	return ret < 0 ? (int)-ret : 0;
}

int sys_fstatvfs(int, struct statvfs *out) {
	long ret = raw_syscall(SYS_statvfs, (long)"/", (long)out);
	return ret < 0 ? (int)-ret : 0;
}

// sysinfo(): not an mlibc sysdep hook — mlibc only declares this under its
// "linux" option (disabled for this port, see sys/sysinfo.h), so there's no
// public wrapper function elsewhere to call into a hook. BusyBox `free`
// and `uptime` call it unconditionally, so this port defines the public
// symbol directly (`extern "C"` gives it global linkage regardless of this
// enclosing namespace). The kernel's sysinfo (#99) fills the Linux struct
// itself: uptime, load averages, total/free RAM in bytes (mem_unit 1) and
// the process count. It used to be reassembled here from statvfs and
// uptime_sec, with the loads and procs left at 0.
// setmntent()/getmntent()/endmntent(): this kernel's mount table is fixed
// at compile time (see kernel/src/fs/mod.rs's MOUNT LAYOUT), so rather than
// parse a real /etc/mtab (which doesn't exist — nothing here writes one),
// this ports that same static table directly. `filename`/`type` are
// ignored — every "file" this could be asked to open is the same table.
// `setmntent`'s return value only needs to be a non-null token the other
// two calls recognize, not a real `FILE*`; nothing dereferences it.
namespace {
struct MntRow { const char *fsname; const char *dir; const char *type; const char *opts; };
constexpr MntRow MNT_TABLE[] = {
	{"initramfs", "/",     "initramfs", "ro"},
	{"devfs",     "/dev",  "devfs",     "ro"},
	{"ramfs",     "/tmp",  "ramfs",     "rw"},
	{"ext2",      "/mnt",  "ext2",      "ro"},
	{"procfs",    "/proc", "procfs",    "ro"},
};
constexpr size_t MNT_TABLE_LEN = sizeof(MNT_TABLE) / sizeof(MNT_TABLE[0]);
size_t g_mnt_index = 0;
struct mntent g_mntent;
} // namespace

extern "C" FILE *setmntent(const char *, const char *) {
	g_mnt_index = 0;
	return (FILE *)&g_mnt_index;
}

extern "C" struct mntent *getmntent(FILE *) {
	if (g_mnt_index >= MNT_TABLE_LEN)
		return nullptr;
	const MntRow &row = MNT_TABLE[g_mnt_index++];
	g_mntent.mnt_fsname = (char *)row.fsname;
	g_mntent.mnt_dir = (char *)row.dir;
	g_mntent.mnt_type = (char *)row.type;
	g_mntent.mnt_opts = (char *)row.opts;
	g_mntent.mnt_freq = 0;
	g_mntent.mnt_passno = 0;
	return &g_mntent;
}

extern "C" int endmntent(FILE *) {
	g_mnt_index = 0;
	return 1;
}

// sched_getaffinity(): <sched.h> declares it for every port, but mlibc
// defines it only under the Linux option (off here, see sys/sysinfo.h).
// Like glibc, clear the part of the caller's mask the kernel didn't write.
extern "C" int sched_getaffinity(pid_t pid, size_t size, cpu_set_t *mask) {
	long ret = raw_syscall(SYS_sched_getaffinity, pid, (long)size, (long)mask);
	if (ret < 0) {
		errno = (int)-ret;
		return -1;
	}
	if ((size_t)ret < size)
		__builtin_memset((char *)mask + ret, 0, size - (size_t)ret);
	return 0;
}

extern "C" int sysinfo(struct sysinfo *info) {
	long ret = raw_syscall(SYS_sysinfo, (long)info);
	if (ret < 0) {
		errno = (int)-ret;
		return -1;
	}
	return 0;
}

// `pid` passes through unconverted — the kernel itself now understands the
// real POSIX overloads (`0` own process group, `<-1` group `-pid`; `-1`
// broadcast is rejected with EINVAL, no permission model to bound it by).
int sys_kill(int pid, int sig) {
	long ret = raw_syscall(SYS_kill, pid, sig);
	return ret < 0 ? (int)-ret : 0;
}

// This kernel's sigaction(13) reads/writes a single `u64` handler address
// at offset 0 of `act`/`oldact` (SIG_DFL=0, SIG_IGN=1, or a handler
// pointer) rather than the full ABI struct — but `sa_handler` (a
// `void (*)(int)`) already IS `struct sigaction`'s first member (see
// include/abi-bits/signal.h), so the raw struct pointer is binary-
// compatible as-is. `sa_mask`/`sa_flags`/`sa_restorer` are silently
// ignored: this kernel injects its own sigreturn trampoline transparently
// (see kernel/src/process/signal.rs), so no restorer needs to be supplied,
// and per-handler blocking during delivery is unconditional rather than
// configurable via sa_mask.
int sys_sigaction(int sig, const struct sigaction *__restrict act,
		struct sigaction *__restrict oldact) {
	long ret = raw_syscall(SYS_sigaction, sig, (long)act, (long)oldact);
	return ret < 0 ? (int)-ret : 0;
}

// sigset_t is a plain uint64_t in this port (abi-bits/signal.h) in Linux's
// layout (bit N-1 = signal N, as mlibc's sigaddset writes it); the kernel
// converts to its own bit-N numbering at the syscall boundary.
int sys_sigprocmask(int how, const sigset_t *__restrict set,
		sigset_t *__restrict old) {
	long ret = raw_syscall(SYS_sigprocmask, how, (long)set, (long)old);
	return ret < 0 ? (int)-ret : 0;
}

// rt_sigsuspend(130) always "fails" with EINTR once a handled signal has
// run — mlibc's sigsuspend() stores the returned errno and returns -1.
// Without this, sigsuspend() was ENOSYS and BusyBox ash's `wait` spun
// forever with every signal blocked (waitproc's sigsuspend loop).
int sys_sigsuspend(const sigset_t *set) {
	long ret = raw_syscall(SYS_rt_sigsuspend, (long)set, (long)sizeof(sigset_t));
	return ret < 0 ? (int)-ret : 0;
}

// pause(34): sigsuspend with the current mask, in the kernel. Always
// EINTR. Without it pause() hit mlibc's missing-sysdep __ensure, which
// returns — so `for (;;) pause();` spun printing it (fb0_test's child).
int sys_pause() {
	long ret = raw_syscall(SYS_pause);
	return ret < 0 ? (int)-ret : 0;
}


// ── AF_UNIX sockets ─────────────────────────────────────────────────────
//
// These map one-to-one onto the kernel's socket syscalls (Linux numbers,
// Linux argument order) — see kernel/src/process/syscall/ipc.rs, which sits
// on the host-tested `usock` crate. AF_UNIX is the only family that exists
// here: there is no network stack, and socket() answers EAFNOSUPPORT for
// anything else rather than pretending.

int sys_socket(int family, int type, int protocol, int *fd) {
	long ret = raw_syscall(SYS_socket, family, type, protocol);
	if (ret < 0)
		return (int)-ret;
	*fd = (int)ret;
	return 0;
}

int sys_socketpair(int domain, int type_and_flags, int proto, int *fds) {
	long ret = raw_syscall(SYS_socketpair, domain, type_and_flags, proto, (long)fds);
	return ret < 0 ? (int)-ret : 0;
}

int sys_bind(int fd, const struct sockaddr *addr_ptr, socklen_t addr_length) {
	long ret = raw_syscall(SYS_bind, fd, (long)addr_ptr, addr_length);
	return ret < 0 ? (int)-ret : 0;
}

int sys_listen(int fd, int backlog) {
	long ret = raw_syscall(SYS_listen, fd, backlog);
	return ret < 0 ? (int)-ret : 0;
}

int sys_connect(int fd, const struct sockaddr *addr_ptr, socklen_t addr_length) {
	long ret = raw_syscall(SYS_connect, fd, (long)addr_ptr, addr_length);
	return ret < 0 ? (int)-ret : 0;
}

// mlibc folds accept() and accept4() into one sysdep; `flags` carries
// SOCK_NONBLOCK/SOCK_CLOEXEC, which the kernel's accept4(288) understands.
int sys_accept(int fd, int *newfd, struct sockaddr *addr_ptr, socklen_t *addr_length, int flags) {
	long ret = raw_syscall(SYS_accept4, fd, (long)addr_ptr, (long)addr_length, flags);
	if (ret < 0)
		return (int)-ret;
	*newfd = (int)ret;
	return 0;
}

ssize_t sys_sendto(int fd, const void *buffer, size_t size, int flags,
		const struct sockaddr *sock_addr, socklen_t addr_length, ssize_t *length) {
	long ret = raw_syscall(SYS_sendto, fd, (long)buffer, (long)size, flags,
			(long)sock_addr, (long)addr_length);
	if (ret < 0)
		return (int)-ret;
	*length = (ssize_t)ret;
	return 0;
}

ssize_t sys_recvfrom(int fd, void *buffer, size_t size, int flags,
		struct sockaddr *sock_addr, socklen_t *addr_length, ssize_t *length) {
	long ret = raw_syscall(SYS_recvfrom, fd, (long)buffer, (long)size, flags,
			(long)sock_addr, (long)addr_length);
	if (ret < 0)
		return (int)-ret;
	*length = (ssize_t)ret;
	return 0;
}

// send()/recv() and sendmsg()/recvmsg() all land here or in sys_sendto
// above; the kernel walks the iovec array and any SCM_RIGHTS control
// message itself.
int sys_msg_send(int fd, const struct msghdr *hdr, int flags, ssize_t *length) {
	long ret = raw_syscall(SYS_sendmsg, fd, (long)hdr, flags);
	if (ret < 0)
		return (int)-ret;
	*length = (ssize_t)ret;
	return 0;
}

int sys_msg_recv(int fd, struct msghdr *hdr, int flags, ssize_t *length) {
	long ret = raw_syscall(SYS_recvmsg, fd, (long)hdr, flags);
	if (ret < 0)
		return (int)-ret;
	*length = (ssize_t)ret;
	return 0;
}

int sys_shutdown(int sockfd, int how) {
	long ret = raw_syscall(SYS_shutdown, sockfd, how);
	return ret < 0 ? (int)-ret : 0;
}

// getsockname(2)/getpeername(2). mlibc hands in the caller's buffer size and
// wants the address's real length back; the kernel's own calls take a single
// in/out socklen_t, so it is staged here.
int sys_sockname(int fd, struct sockaddr *addr_ptr, socklen_t max_addr_length,
		socklen_t *actual_length) {
	socklen_t len = max_addr_length;
	long ret = raw_syscall(SYS_getsockname, fd, (long)addr_ptr, (long)&len);
	if (ret < 0)
		return (int)-ret;
	*actual_length = len;
	return 0;
}

int sys_peername(int fd, struct sockaddr *addr_ptr, socklen_t max_addr_length,
		socklen_t *actual_length) {
	socklen_t len = max_addr_length;
	long ret = raw_syscall(SYS_getpeername, fd, (long)addr_ptr, (long)&len);
	if (ret < 0)
		return (int)-ret;
	*actual_length = len;
	return 0;
}

int sys_getsockopt(int fd, int layer, int number,
		void *__restrict buffer, socklen_t *__restrict size) {
	long ret = raw_syscall(SYS_getsockopt, fd, layer, number, (long)buffer, (long)size);
	return ret < 0 ? (int)-ret : 0;
}

int sys_setsockopt(int fd, int layer, int number,
		const void *buffer, socklen_t size) {
	long ret = raw_syscall(SYS_setsockopt, fd, layer, number, (long)buffer, (long)size);
	return ret < 0 ? (int)-ret : 0;
}

#endif // MLIBC_BUILDING_RTLD

} // namespace mlibc
