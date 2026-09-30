---
name: linux-abi
description: Playbook for making the kernel speak Linux's ABI so unmodified musl/Rust-std/other Linux binaries run (workstream "G1"): running a real `x86_64-unknown-linux-musl` Rust std program on the kernel, the raw-syscall C test recipe, proving kernel-side logic by sabotage, and the traps that cost time (opt-level-0 stack copies, IF=0 rules, fork masking PTE bugs, compilers folding UB). Use when adding or changing a syscall, a flag, a signal or wait behaviour, the ELF loader, the fd table, or when a Linux program misbehaves on the kernel. Keywords: musl, rust std, linux abi, clone, sigaction, ucontext, wait4, cloexec, mprotect, static-pie, poll, ENOSYS.
---

# Linux ABI for std (G1)

State and the list of what is still missing: `docs/reference/syscalls.md` (rows, ABI notes) and the memory note `rust_std_gaps`. Rule of thumb: **change the kernel to Linux's ABI and, when mlibc disagreed, change mlibc's sysdep/header too** (then rebuild everything, `userspace-programs` skill). Do not add heuristics that guess which ABI a caller means.

## Running a real Rust std program on the kernel

1. Build with the repo's pinned nightly: run `rustc` **from inside the repo dir** (outside it the default toolchain has no musl std): `rustc --target x86_64-unknown-linux-musl -O prog.rs -o prog` (static-pie by default).
2. Put it on the disk image without a rebuild: `debugfs -w -R "write prog /prog" disk.img` (`rm /prog` first). It runs as `/mnt/prog`.
3. Boot headless with `QEMU_DEBUG_SMP=4 scripts/qemu-debug.sh start --no-build`, `send "/mnt/prog; echo END=\$?"`, `wait-for 'END=[0-9]'` (`qemu-debug` skill). Print markers, not a wall of log: `grep -vE "^  |^Creat|^Sched|^sys_exec|^ELF|SLAB" serial.log | tail`.
4. To see which syscall a program needs first, make the dispatcher log ENOSYS temporarily (`SyscallNumber::from_u64` → `None`).

## Testing a syscall: the raw C test

- `userspace/c/<thing>_test.c`, added to `DISK_C_PROGRAMS`; raw `syscall` wrappers (`sc(nr, a, b, c, ...)`) when the point is the kernel's ABI, mlibc calls when the point is the whole path. Look at `linux_abi_test.c`, `cloexec_test.c`, `sigabi_test.c`, `sigsegv_test.c`, `pipe_poll_test.c`, `fdlimit_test.c`.
- A fault, a fatal signal or a state you must not inherit: **`fork()` a child** and read its `waitpid` status. Threads to interleave with a spin loop: `pthread_create` + `tgkill`, or a raw `clone` with a small asm stub (`test_clone` in `linux_abi_test.c`).
- **Prove each test by sabotage** (`kernel-testing` skill): break one invariant in the kernel, rebuild, run, expect FAIL or a hang, restore. A hang counts as a detection; a test that still passes means the test is weak. Half the tests here needed a second, stronger check:
  - `fork` masks PTE bugs: the child's PTEs are rebuilt from the VMA flags, so "the parent's PTE was updated" must be checked in a child that maps and lowers **after** the fork (`mprotect_test`).
  - A refault inside a handler died anyway (by stack exhaustion, later): make the second handler `_exit(77)` so only wrong delivery is visible.
  - `1 / z` with `z = 0` is undefined behaviour and clang folds it into a select; `base != 0` on the address of an object is folded to true. Use inline asm (`div`) or compare against a constant.
- Scripted runner used in the sessions (recreate it in the scratchpad): build, `qemu-debug.sh stop/start --no-build`, wait for `# `, `sleep 5`, then for each test `send "/mnt/bin/T; echo END_T=\$?"`, `wait-for END_T=[0-9]`, and grep `\[fb\].*FAIL`. Never `pkill -f qemu-system` from a command whose own text contains it (it kills the shell); use `qemu-debug.sh stop`.
- After any change to a path shared by all processes (fault entry, fd table, signals, exec, poll): `scripts/run-kernel-tests.sh`, ~15 userspace tests (`lifecycle`, `jobctl`, `pthread`, `sigsuspend`, `pipe_multi`, `fork_exec`, `socket`, `pty`), and `QEMU_DEBUG_SMP=4 scripts/boot-matrix.sh 4 5`.

## Traps that cost time

- **Big values by value overflow the stack at opt-level 0.** An inline `[T; 256]` in a struct that goes through `Mutex::new`/`Arc::new`/`clone`, or a per-pid `[usize; 256]` as a `BTreeMap` value (a node holds 11 of them), gave a double fault creating PID 1 and a kernel-stack overflow. Use `Vec`/`Box`, sparse maps keyed `(pid, fd)`. A `DOUBLE FAULT` panic: `addr2line -f -C -e kernel/target/x86_64-unknown-none/debug/kernel <rip - 0x10000000000>` names the function.
- **IF=0 and lock rules** (CLAUDE.md) apply to every new path: `sys_exit` and the death path run with IF=0 and must not call helpers that `sti` (`futex_wake_irq_off`, `Scheduler::clear_child_tid`); a wake from a pipe/socket needs `without_interrupts` around `POLL_WAITERS`; `Drop` of a file handle must not run under `SCHEDULER`; closing pipe/socket ends needs IF=0 (`exec`'s close-on-exec loop panicked without it). Inside `Scheduler` code, wake with `self.wake_with_retval`, and take `FUTEX_WAITERS` only (order: scheduler → `FUTEX_WAITERS`).
- **A hand-written exception entry** builds a full `TrapFrame` (`fault_entry!` in `init/devices.rs`): `cld`, `xchg rax,[rsp]`, pushes, call, pops, `iretq`. A fault in user mode can then run a signal handler by rewriting the frame; a frame push onto a bad stack must fail cleanly (`prepare_user_write` returns false → kill), never write and fault in the kernel.
- **Threads are processes** here (own pid = the tid, shared `Arc<AddressSpace>`, `tgid` = the leader's pid: `getpid`/`kill`/`waitpid`/`/proc/self` use the tgid, `gettid`/`tkill` the pid). A thread group = the processes with `Arc::ptr_eq` address spaces (`exit_group`, `kill_thread_group`). A fatal signal must kill the group and the leader must carry the real signal.
- **Status words and flag values must be Linux's on both sides**: wait status (`code << 8`, signal in the low 7 bits), `WNOHANG=1`, `SA_*`, `O_CLOEXEC`, `stack_t`, `siginfo_t`, `ucontext_t`. When mlibc's header differs, replace the header with `mlibc/abis/linux/...` and rebuild libc, busybox, ncurses (`rm -rf build-libtinfo/prefix`), doom, quake.
- A syscall whose result is written by a *waker* (poll, pipes) needs the buffer pre-translated within one page; `poll` arrays that straddle a page are `EFAULT`.
- Blocking-syscall restart rewinds `rip -= 2`; a syscall that returns `EINTR`/restarts must go through `process::wait` (see `processes-and-scheduling.md`).

## Where things are

`process/syscall/{process_ctl,signal,sync,fs,poll}.rs` (clone/exit_group/exec, rt_sigaction/sigaltstack, futex, open/fcntl/pipe2/dup3/mprotect, poll/epoll), `process/signal.rs` (frame, `deliver_fault`), `process/scheduler.rs` (`kill_current`, `kill_thread_group`), `process/file.rs` (fd table), `process/pipe.rs` (`PIPES` registry, `poll_mask`), `memory/{vma,address_space,elf_loader}.rs` (split/merge, mprotect, static-pie), `init/devices.rs` (fault entries), `mlibc-port/constanos-sysdeps/generic/{generic.cpp,thread_entry.S}` (`__constanos_clone`, restorers, sigaction sysdep).

## Still missing (check `rust_std_gaps` first)

`rem` of `nanosleep`/`clock_nanosleep` on EINTR (needs a hook in `process::wait`), `si_uid` (no uid model) and `CLD_CONTINUED`, `CLONE_VFORK` does not suspend the parent, epoll instances hold 16 watches.
