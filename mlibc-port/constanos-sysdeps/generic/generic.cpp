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
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <termios.h>

namespace {

inline long raw_syscall(long nr, long a1 = 0, long a2 = 0, long a3 = 0,
                         long a4 = 0, long a5 = 0) {
	long ret;
	register long r10 asm("r10") = a4;
	register long r8  asm("r8")  = a5;
	asm volatile ("syscall"
			: "=a"(ret)
			: "a"(nr), "D"(a1), "S"(a2), "d"(a3), "r"(r10), "r"(r8)
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
// SYS_sigreturn(15) is never called directly by userspace — only the
// kernel-mapped trampoline page uses it (see kernel/src/memory/
// signal_trampoline.rs); mlibc's sigaction() doesn't need to know it
// exists, since this kernel injects the trampoline transparently instead
// of relying on a userspace-supplied sa_restorer.
constexpr long SYS_poll = 7;
constexpr long SYS_lseek = 8;
constexpr long SYS_mmap = 9;
constexpr long SYS_getcwd = 79;
constexpr long SYS_chdir = 80;
constexpr long SYS_rename = 82;
constexpr long SYS_mkdir = 83;
constexpr long SYS_rmdir = 84;
constexpr long SYS_unlink = 87;
constexpr long SYS_lstat = 6;
constexpr long SYS_readlink = 89;
constexpr long SYS_dup = 32;
constexpr long SYS_dup2 = 33;
constexpr long SYS_fcntl = 72;
constexpr long SYS_pipe = 22;
constexpr long SYS_munmap = 11;
constexpr long SYS_ioctl = 16;
constexpr long SYS_nanosleep = 35;
constexpr long SYS_getpid = 39;
constexpr long SYS_clone = 56;
constexpr long SYS_fork = 57;
constexpr long SYS_execve = 59;
constexpr long SYS_exit = 60;
constexpr long SYS_waitpid = 61;
constexpr long SYS_kill = 62;
constexpr long SYS_setpgid = 109;
constexpr long SYS_setsid = 112;
constexpr long SYS_getpgid = 121;
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

constexpr long ARCH_SET_FS = 0x1002;
constexpr long FUTEX_WAIT = 0;
constexpr long FUTEX_WAKE = 1;

} // namespace

// Static-linking-only substitute for the dynamic linker's per-module handle.
// We never load shared objects, so a single dummy definition is sufficient
// to satisfy __cxa_atexit() call sites pulled in by static C++ initializers
// (e.g. stdio's buffering globals).
extern "C" void *__dso_handle = (void *)&__dso_handle;

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

// Backs stat()/lstat()/fstat() alike (mlibc funnels all three through this
// one entry point, discriminated by `fsfdt`). `lstat()` is `fsfd_target::path`
// with `AT_SYMLINK_NOFOLLOW` set in `flags` — real symlink support now
// exists kernel-side (see kernel/src/fs/vfs.rs's `resolve`/`resolve_no_follow`),
// so this now genuinely routes to the no-follow SYS_lstat instead of aliasing
// plain stat(). fd_path/none (the *at() directory-relative forms) aren't
// wired up: this kernel's stat/fstat only take an absolute-ish path or a
// bare fd, no dirfd+relative-path syscall.
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

// No real output buffering or discardable input queue exists beyond
// `keyboard_buffer::KEYBOARD_BUFFER` (which nothing here usefully
// truncates) — these are all no-ops that report success, same spirit as
// `sys_brk` telling mlibc "nothing to do here, you already got what you
// need another way".
int sys_tcdrain(int) { return 0; }
int sys_tcflow(int, int) { return 0; }
int sys_tcflush(int, int) { return 0; }

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

int sys_vm_map(void *hint, size_t size, int prot, int flags,
		int fd, off_t, void **window) {
	__ensure(flags & MAP_ANONYMOUS);
	(void)fd;
	long ret = raw_syscall(SYS_mmap, (long)hint, (long)size, prot,
			MAP_ANONYMOUS, -1);
	if (ret < 0)
		return (int)-ret;
	*window = (void *)ret;
	return 0;
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

int sys_sleep(time_t *secs, long *nanos) {
	long ns = (*secs) * 1000000000L + *nanos;
	raw_syscall(SYS_nanosleep, ns);
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

pid_t sys_getppid() {
	// Not tracked by this kernel; harmless placeholder.
	return 1;
}

// No real user/group model exists — this kernel is single-user, everything
// runs as an implicit root (uid==euid==gid==egid==0). ash's startup (and
// anything else calling getuid()/geteuid()) needs these to not be missing
// sysdeps, not for the value to mean anything beyond "not a setuid binary".
uid_t sys_getuid() { return 0; }
uid_t sys_geteuid() { return 0; }
gid_t sys_getgid() { return 0; }
gid_t sys_getegid() { return 0; }

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

// sigset_t is already a plain uint64_t in this port (abi-bits/signal.h) —
// matches this kernel's 32-signal bitmask directly, no conversion needed.
int sys_sigprocmask(int how, const sigset_t *__restrict set,
		sigset_t *__restrict old) {
	long ret = raw_syscall(SYS_sigprocmask, how, (long)set, (long)old);
	return ret < 0 ? (int)-ret : 0;
}

#endif // MLIBC_BUILDING_RTLD

} // namespace mlibc
