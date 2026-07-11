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
#include <mlibc/thread-entry.hpp>
#include <mlibc/tcb.hpp>
#include <errno.h>
#include <dirent.h>
#include <fcntl.h>
#include <limits.h>
#include <sys/mman.h>

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
constexpr long SYS_lseek = 8;
constexpr long SYS_mmap = 9;
constexpr long SYS_munmap = 11;
constexpr long SYS_ioctl = 16;
constexpr long SYS_nanosleep = 35;
constexpr long SYS_getpid = 39;
constexpr long SYS_clone = 56;
constexpr long SYS_fork = 57;
constexpr long SYS_execve = 59;
constexpr long SYS_exit = 60;
constexpr long SYS_arch_prctl = 158;
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
	long ret = raw_syscall(SYS_ioctl, fd, 0x5401, 0);
	return ret < 0 ? (int)-ret : 0;
}

int sys_seek(int fd, off_t offset, int whence, off_t *new_offset) {
	long ret = raw_syscall(SYS_lseek, fd, offset, whence);
	if (ret < 0)
		return (int)-ret;
	*new_offset = ret;
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

int sys_execve(const char *path, char *const[], char *const[]) {
	// This kernel's exec() only takes a program name — no argv/envp.
	long ret = raw_syscall(SYS_execve, (long)path);
	return ret < 0 ? (int)-ret : 0;
}

pid_t sys_getpid() {
	return (pid_t)raw_syscall(SYS_getpid);
}

pid_t sys_getppid() {
	// Not tracked by this kernel; harmless placeholder.
	return 1;
}

#endif // MLIBC_BUILDING_RTLD

} // namespace mlibc
