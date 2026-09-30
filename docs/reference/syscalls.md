# Syscalls

Code: `kernel/src/process/syscall/` (dispatcher, `SyscallNumber` is the authoritative list), `signal.rs`, `wait.rs`.

## Entry and locking

- Syscalls enter through the `syscall` instruction (`IA32_LSTAR` → `syscall_entry_fast`, set up in `process/tss.rs`), not an IDT gate. The stub does `cld`, `swapgs` to load the kernel stack, pushes every GPR into a `TrapFrame` at the top of the kernel stack, calls the dispatcher, writes the return value into the saved RAX, and returns with `sysretq`/`iretq`.
- The current syscall's frame is `syscall::current_tf_ptr()`, computed from the kernel stack top. It is never kept in a global.
- `with_current_process`/`with_scheduler` disable interrupts, then lock. **Don't use them around anything that can take `SCHEDULER` again**: `sys_close`/`sys_dup2` (a `Drop` may lock it), `getdents64` (procfs lists pids through `all_pids()`), `sys_read`'s generic path. Pattern instead: clone the fd table's `Arc`, drop the lock, then call the handle.
- **Blocking syscalls restart** by rewinding the saved `rip` by 2 (the width of `syscall`); `rax` still holds the number. See `processes-and-scheduling.md`.

## ABI quirks (differences from Linux)

- `nanosleep` takes plain nanoseconds, not a `timespec`.
- `sigset_t` arrives in Linux layout (bit N-1 = signal N) and is shifted to the kernel's bit-N masks by `signal::mask_from_user`.
- termios/winsize use this port's own layout (`tty` crate), not Linux's.
- Custom numbers above the Linux range: 400 `uptime_ms`, 401 `uptime_sec`, 402 `meminfo_kb`, 403 `kdebug_ctl`, 404 `statvfs`.

## Table

| Nr | Name | Notes |
|----|------|-------|
| 0/1/2/3 | read/write/open/close | |
| 4/5/6 | stat/fstat/lstat | `lstat` does not follow a final symlink. `fstat` runs `FileHandle::stat` under the scheduler lock |
| 7 | poll | ≤64 entries in an array that does not straddle a page; a negative fd is skipped, an fd past the highest open one is `POLLNVAL`. Real readiness for sockets, stdin, ptys, pipes, `/dev/input/event*` (`FileHandle::event_source`); other devices are always ready. `POLLHUP`/`POLLERR` are reported even if not asked for (by epoll too) |
| 8 | lseek | |
| 9/11 | mmap/munmap | Private anonymous, `MAP_SHARED` of a memfd, or `MAP_SHARED\|MAP_ANONYMOUS`. A nonzero `addr` is treated as `MAP_FIXED` (and fails over an existing mapping). `prot` 0 is a real `PROT_NONE`. `munmap` takes any page-aligned range: cuts VMAs, spans several, holes are fine |
| 10 | mprotect | Splits VMAs at the range's ends and rejoins equal neighbours; hole in the range → `ENOMEM`. A `Huge2M` VMA can only be cut on 2 MiB boundaries. `PROT_EXEC` is ignored (NX is off) |
| 12 | brk | |
| 13/14/15 | rt_sigaction/rt_sigprocmask/rt_sigreturn | **Linux's ABI** (`{handler, flags, restorer, mask}`, sigsetsize 8; `abi-bits/signal.h` is Linux's). Honoured: `SA_SIGINFO` (handler gets `siginfo*` and a Linux `ucontext*`), `SA_RESTORER`, `SA_ONSTACK`, `SA_RESTART`, `SA_NODEFER`, `SA_RESETHAND`, `sa_mask`. Without `SA_RESTORER` the handler returns through a fixed trampoline page. `rt_sigreturn` takes the registers and mask back from the (editable) `ucontext`. fork and clone inherit dispositions, flags and the mask. Faults of user code reach handlers too: page fault and `#GP` → `SIGSEGV` (`si_code` `SEGV_MAPERR`/`SEGV_ACCERR`, `SI_KERNEL` for `#GP`, `si_addr` set), `#UD` → `SIGILL`, `#DE` → `SIGFPE`; with no handler, with the signal blocked (a fault inside its own handler) or with no room for the frame, the process dies of that signal. `si_pid` is always 0 |
| 16 | ioctl | termios, `TIOCGWINSZ`, `TIOCG/SPGRP`. On any fd: `FIONBIO`, `FIOCLEX`/`FIONCLEX`. On a pty the handle answers every tty ioctl. `/dev/fb`: `FBIO_BLIT` 0x4642_0001. `/dev/fb0`: `FBIO_GET_INFO` 0x4642_0010, `FBIO_FLUSH` 0x4642_0011 |
| 20 | writev | |
| 21 | access | `F_OK`/`R_OK`/`X_OK` = "the path resolves". `W_OK` really probes: opens `O_WRONLY` and writes 0 bytes |
| 22/293 | pipe/pipe2 | `pipe2`: `O_CLOEXEC`, `O_NONBLOCK`. A non-blocking end fails a would-block read/write with `EAGAIN`. `poll`/`epoll` report real readiness (`process::pipe::poll_mask`, registry `PIPES` by number) and are woken by writes, reads, and closes of either end |
| 24 | yield | |
| 32/33/292 | dup/dup2/dup3 | Shared offset. `dup`/`dup2` clear `FD_CLOEXEC` on the new fd; `dup3` sets it with `O_CLOEXEC` (`oldfd == newfd` → `EINVAL`) |
| 34 | pause | `rt_sigsuspend` with the current mask; always `EINTR` |
| 35 | nanosleep | hrtimer; `EINTR` on a signal |
| 39/110 | getpid/getppid | ppid is 1 after reparenting, 0 for PID 1 |
| 41–55, 288 | socket … accept4 | AF_UNIX only (`AF_INET` → `EAFNOSUPPORT`). See `ipc.md` |
| 56/57 | clone/fork | **clone is Linux's** `(flags, stack, ptid, ctid, tls)`. `CLONE_THREAD` (needs `VM`+`SIGHAND`): a thread that shares the address space and fds, resumes after the `syscall` with the caller's registers and `rax=0`, honours `SETTLS`, `PARENT_SETTID`, `CHILD_SETTID`, `CHILD_CLEARTID`. Without `CLONE_THREAD`: a COW fork on `stack` (musl's `posix_spawn`; the parent is not suspended). A thread is a process with its own pid: no tgid, `getpid()` returns the tid. mlibc's `sys_clone` calls it through `__constanos_clone` (`thread_entry.S`) |
| 59 | exec | `(path, argv, envp)`. Closes the `FD_CLOEXEC` fds once the image has loaded (a failed exec keeps them). `open(O_CLOEXEC)`, `pipe2`, `dup3`, `memfd_create(MFD_CLOEXEC)` and `SOCK_CLOEXEC` set it. Resolved through the VFS with symlinks followed. Caught signals go back to `SIG_DFL`; ignored stay ignored; mask and pending carry over |
| 60 | exit | See Process death in `processes-and-scheduling.md` |
| 61 | waitpid | Linux's `wait4` ABI: `WNOHANG`=1, `WUNTRACED`=2, status = `code<<8` / signal in the low 7 bits / `0x7f\|sig<<8` for a stop (`Process::wait_status_word`); the rusage argument is ignored. POSIX pid forms. No matching child → `ECHILD`, even with `WNOHANG` |
| 62 | kill | pid >0, 0, <-1. Interrupts an interruptible wait. `SIGCONT` and `SIGKILL` resume a stopped target. Signal 0 only probes that the pid exists (`ESRCH` otherwise) |
| 72 | fcntl | `F_DUPFD`/`F_DUPFD_CLOEXEC`, `F_GETFD`/`F_SETFD` (`FD_CLOEXEC`), `F_GETFL`/`F_SETFL` (`O_NONBLOCK` only) |
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
| 131 | sigaltstack | Per thread, Linux's `stack_t`. `SA_ONSTACK` handlers run on it; `SS_DISABLE`, `SS_ONSTACK` in `old_ss`, `EPERM` while on it, `ENOMEM` under 2048 bytes. `fork` copies it, a thread starts without one, `exec` clears it |
| 186/200/234 | gettid/tkill/tgkill | A thread is a process with its own pid, so tid = pid and `tkill` = `kill` on it; `tgkill` does not check the group |
| 202 | futex | `WAIT` (relative timeout, `ETIMEDOUT`), `WAIT_BITSET` (absolute `CLOCK_MONOTONIC`, or `CLOCK_REALTIME` with the flag; Rust `std` waits with it), `WAKE`, `WAKE_BITSET`, `REQUEUE`/`CMP_REQUEUE`. Not `WAKE_OP`/PI. A timeout already past returns `ETIMEDOUT` without arming a timer |
| 230 | clock_nanosleep | Linux ABI (`timespec`), unlike 35. `TIMER_ABSTIME` on realtime/monotonic/boottime; `rem` is never written, so an interrupted sleep restarts with the whole time when retried |
| 318 | getrandom | `crate::random` (ChaCha20, seeded on first use from RDSEED/RDRAND, TSC, clock, jitter). Never blocks; flags validated. Also `/dev/urandom`, `/dev/random` |
| 204 | sched_getaffinity | The scheduling CPUs; returns 8. No setaffinity |
| 213/232/233 | epoll_create/wait/ctl | Shares poll's readiness |
| 217 | getdents64 | `linux_dirent64` |
| 218 | set_tid_address | Stores `Process::clear_child_tid`; `Scheduler::kill_current` (every way of dying) zeroes it and futex-wakes it. `exec` clears it |
| 231 | exit_group | SIGKILLs every process sharing the caller's address space, then `exit`. The leader (what the parent waits for) is flagged `group_exited`, so `wait_status_word` reports `exit(status)` even though SIGKILL ends it; the caller is flagged too, so its threads' SIGKILL deaths (`kill_thread_group`) cannot tag it. mlibc's `sys_exit` is `exit_group` (`exit()` from a thread ends the process), `sys_thread_exit` is plain exit. A fatal signal (or fault) in any thread ends the whole group the same way (`Scheduler::kill_thread_group`), and the leader's parent sees that signal |
| 228/229 | clock_gettime/getres | `REALTIME` = RTC at boot + uptime. `MONOTONIC`/`BOOTTIME`/… = uptime. CPU-time clocks from `exec_ns`. Resolution 1 ns |
| 280 | utimensat | `UTIME_NOW`/`OMIT`, `AT_SYMLINK_NOFOLLOW`, NULL path = futimens. A relative path with a real dirfd → `ENOSYS` |
| 319 | memfd_create | |
| 403 | kdebug_ctl | cmd 0 get mask, 1 set subsystem on/off, 2 panic, 3 TLB self-test, 4 idle mode (`hlt`/`c2`). Backs `kdebug` |
| 404 | statvfs | Every mount reports the buddy allocator's totals |
