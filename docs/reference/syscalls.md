# Syscalls

Code: `kernel/src/process/syscall/` (dispatcher, `SyscallNumber` is the authoritative list), `signal.rs`, `wait.rs`.

## Entry and locking

- Syscalls enter through the `syscall` instruction (`IA32_LSTAR` → `syscall_entry_fast`, set up in `process/tss.rs`), not an IDT gate. The stub does `cld`, `swapgs` to load the kernel stack, pushes every GPR into a `TrapFrame` at the top of the kernel stack, calls the dispatcher, writes the return value into the saved RAX, and returns with `sysretq`/`iretq`.
- The current syscall's frame is `syscall::current_tf_ptr()`, computed from the kernel stack top. It is never kept in a global.
- `with_current_process`/`with_scheduler` disable interrupts, then lock. **Don't use them around anything that can take `SCHEDULER` again**: `sys_close`/`sys_dup2` (a `Drop` may lock it), `getdents64` (procfs lists pids through `all_pids()`), `sys_read`'s generic path. Pattern instead: clone the fd table's `Arc`, drop the lock, then call the handle.
- **Blocking syscalls restart** by rewinding the saved `rip` by 2 (the width of `syscall`); `rax` still holds the number. See `processes-and-scheduling.md`.

## ABI quirks (differences from Linux)

- `nanosleep` takes plain nanoseconds, not a `timespec`.
- `struct sigaction`: `sa_flags` is at offset 16, and `SA_RESTART` = `1<<3`. Only `SA_RESTART` is honoured.
- `sigset_t` arrives in Linux layout (bit N-1 = signal N) and is shifted to the kernel's bit-N masks by `signal::mask_from_user`.
- termios/winsize use this port's own layout (`tty` crate), not Linux's.
- `WNOHANG` is 2 (Linux: 1).
- Custom numbers above the Linux range: 400 `uptime_ms`, 401 `uptime_sec`, 402 `meminfo_kb`, 403 `kdebug_ctl`, 404 `statvfs`.

## Table

| Nr | Name | Notes |
|----|------|-------|
| 0/1/2/3 | read/write/open/close | |
| 4/5/6 | stat/fstat/lstat | `lstat` does not follow a final symlink. `fstat` runs `FileHandle::stat` under the scheduler lock |
| 7 | poll | ≤16 fds. Real readiness for sockets, stdin, ptys, `/dev/input/event*` (`FileHandle::event_source`); other devices are always ready. `POLLHUP`/`POLLERR` are reported even if not asked for (by epoll too) |
| 8 | lseek | |
| 9/11 | mmap/munmap | Private anonymous, `MAP_SHARED` of a memfd, or `MAP_SHARED\|MAP_ANONYMOUS`. A nonzero `addr` is treated as `MAP_FIXED`. `munmap` must name one whole VMA |
| 12 | brk | |
| 13/14/15 | sigaction/sigprocmask/sigreturn | fork and clone inherit dispositions, `SA_RESTART` and the mask |
| 16 | ioctl | termios, `TIOCGWINSZ`, `TIOCG/SPGRP`. On a pty the handle answers every tty ioctl. `/dev/fb`: `FBIO_BLIT` 0x4642_0001. `/dev/fb0`: `FBIO_GET_INFO` 0x4642_0010, `FBIO_FLUSH` 0x4642_0011 |
| 20 | writev | |
| 21 | access | `F_OK`/`R_OK`/`X_OK` = "the path resolves". `W_OK` really probes: opens `O_WRONLY` and writes 0 bytes |
| 22 | pipe | |
| 24 | yield | |
| 32/33 | dup/dup2 | Shared offset |
| 34 | pause | `rt_sigsuspend` with the current mask; always `EINTR` |
| 35 | nanosleep | hrtimer; `EINTR` on a signal |
| 39/110 | getpid/getppid | ppid is 1 after reparenting, 0 for PID 1 |
| 41–55, 288 | socket … accept4 | AF_UNIX only (`AF_INET` → `EAFNOSUPPORT`). See `ipc.md` |
| 56/57 | clone/fork | Thread (shares address space and fds) / COW fork |
| 59 | exec | `(path, argv, envp)`. Resolved through the VFS with symlinks followed. Caught signals go back to `SIG_DFL`; ignored stay ignored; mask and pending carry over |
| 60 | exit | See Process death in `processes-and-scheduling.md` |
| 61 | waitpid | POSIX pid forms, `WNOHANG`/`WUNTRACED`, `WIFSIGNALED`. No matching child → `ECHILD`, even with `WNOHANG` |
| 62 | kill | pid >0, 0, <-1. Interrupts an interruptible wait. `SIGCONT` and `SIGKILL` resume a stopped target. Signal 0 only probes that the pid exists (`ESRCH` otherwise) |
| 72 | fcntl | Only `F_DUPFD`/`F_DUPFD_CLOEXEC` do something |
| 77 | ftruncate | memfds only (`EINVAL` otherwise); shrinking a mapped one → `EBUSY` |
| 82/83/84/87 | rename/mkdir/rmdir/unlink | ramfs and ext2; `EROFS` elsewhere |
| 88/89 | symlink/readlink | Target stored verbatim. ramfs and ext2 |
| 90/91 | chmod/fchmod | Real on ext2; elsewhere only checks the path/fd |
| 98 | getrusage | `ru_utime`/`ru_stime` only |
| 99 | sysinfo | Linux struct: uptime, loads `<<16`, RAM in bytes, procs |
| 100 | times | Ticks of 100 Hz; returns uptime in ticks |
| 109/121/112/124 | setpgid/getpgid/setsid/getsid | POSIX sessions (`ESRCH`/`EPERM` rules). PID 1 leads session 1. Test: `session_test` |
| 130 | rt_sigsuspend | Sleeps until a signal that runs a handler, terminates or stops; returns `EINTR` with the old mask restored (`saved_sigmask`). Backs ash's `wait` |
| 158 | arch_prctl | `ARCH_SET_FS` |
| 162 | sync | Flushes the kernel log to the USB log partition (ext2 writes are synchronous). Errors: `ENODEV`/`EBUSY`/`EIO` |
| 169 | reboot | Flushes the log, then resets: FADT `RESET_REG` → port 0xCF9 → 8042 0xFE → triple fault (`reboot.rs`). `HALT`/`POWER_OFF` just stop |
| 131 | sigaltstack | Accepts and reports `SS_DISABLE`; handlers still run on the interrupted stack (`SA_ONSTACK` is not honoured) |
| 186/200/234 | gettid/tkill/tgkill | A thread is a process with its own pid, so tid = pid and `tkill` = `kill` on it; `tgkill` does not check the group |
| 202 | futex | `WAIT` (relative timeout, `ETIMEDOUT`), `WAIT_BITSET` (absolute `CLOCK_MONOTONIC`, or `CLOCK_REALTIME` with the flag; Rust `std` waits with it), `WAKE`, `WAKE_BITSET`, `REQUEUE`/`CMP_REQUEUE`. Not `WAKE_OP`/PI. A timeout already past returns `ETIMEDOUT` without arming a timer |
| 230 | clock_nanosleep | Linux ABI (`timespec`), unlike 35. `TIMER_ABSTIME` on realtime/monotonic/boottime; `rem` is never written, so an interrupted sleep restarts with the whole time when retried |
| 318 | getrandom | `crate::random` (ChaCha20, seeded on first use from RDSEED/RDRAND, TSC, clock, jitter). Never blocks; flags validated. Also `/dev/urandom`, `/dev/random` |
| 204 | sched_getaffinity | The scheduling CPUs; returns 8. No setaffinity |
| 213/232/233 | epoll_create/wait/ctl | Shares poll's readiness |
| 217 | getdents64 | `linux_dirent64` |
| 218 | set_tid_address | stub |
| 228/229 | clock_gettime/getres | `REALTIME` = RTC at boot + uptime. `MONOTONIC`/`BOOTTIME`/… = uptime. CPU-time clocks from `exec_ns`. Resolution 1 ns |
| 280 | utimensat | `UTIME_NOW`/`OMIT`, `AT_SYMLINK_NOFOLLOW`, NULL path = futimens. A relative path with a real dirfd → `ENOSYS` |
| 319 | memfd_create | |
| 403 | kdebug_ctl | cmd 0 get mask, 1 set subsystem on/off, 2 panic, 3 TLB self-test, 4 idle mode (`hlt`/`c2`). Backs `kdebug` |
| 404 | statvfs | Every mount reports the buddy allocator's totals |
