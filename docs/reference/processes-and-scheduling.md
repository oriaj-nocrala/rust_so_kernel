# Processes and Scheduling

Code: `kernel/src/process/` (`scheduler.rs`, `timer_preempt.rs`, `trapframe.rs`, `fpu.rs`, `tss.rs`, `wait.rs`, `signal.rs`), crate `sched/` (host tests: `cd sched && cargo test`). Background: `docs/smp/smp-plan.md` (stage 7), `docs/sched/`.

## Process

- `Process` holds: pid, state, base and effective priority (0–10), name, `Box<TrapFrame>`, kernel stack, `Arc<AddressSpace>`, fd table, sid/pgid, ctty, signal state, times.
- **A thread is a process that shares its `AddressSpace`.** `Process::pid` is the tid, `Process::tgid` the thread-group id (the leader's pid, what `getpid`, `kill`, `waitpid`, a child's `parent_pid` and `/proc/self` use). Kernel code that finds a group compares `tgid` (`exit_group`, `kill_thread_group`, CPU-time sums, `fold_into_leader`), not the `AddressSpace`: a `clone(CLONE_VM)` without `CLONE_THREAD` shares the `Arc<AddressSpace>` but is its own process and group. A thread's `parent_pid` is its group's parent.
- Names:
  - `name` (Linux `comm`, 15 bytes) = basename of the path *as passed* to exec, before symlinks are followed.
  - `exe_name` = the canonical resolved path (`/proc/<pid>/exe`).
  - `cmdline` = argv.
  - fork/clone inherit all three. `set_name` zeroes the whole field first.

## Scheduler

- **One scheduler for all CPUs**: a single `SCHEDULER` lock, one `sched::SchedCore<Process>`, plus `running[cpu]` and `idle[cpu]`. `running_ref`/`running_mut`/`current_pid` refer to *this* CPU.
- `sched` holds the policy: run queues `[0..=10]` (Ready only), a wait queue (Blocked + Zombie), decay on preemption, aging, and quantum `BASE_QUANTUM + eff_pri * BONUS`. `impl SchedEntity for Process` is a pure field bridge. Known aging/starvation defects: `sched/src/lib.rs` "Known limitations" and `docs/sched/sched-bugs-plan.md`.
- Idle processes: one per CPU, all pid 0, never queued, hidden from `/proc`.
- **The tick runs on every scheduling CPU**, and time slices are per CPU. **Global work runs on CPU 0 only**: cursor blink, USB poll, `TICK_COUNT`, hrtimers, aging, load average.
- When something becomes Ready while a CPU idles, `kick_idle` sends the **reschedule IPI** (vector 0xF2), to this CPU first.
- `CONSTANOS_NOSMP=1` at build time: only CPU 0 schedules.
- `/proc/kdebug` `sched:` shows per-CPU pid, switch counts, busy/idle ticks, IPIs, `leaving_skips`, and `invariants=` (`check_invariants_with_running` run on the live scheduler).

## Context switch

- The timer ISR (hand-written asm, pushes all GPRs) calls `timer_tick`. `switch_to_next()` returns the next `TrapFrame`, and `jump_to_trapframe` restores every register and does `iretq`. Kill and switch go through the same path.
- **FPU/SSE/AVX**: `Process::fpu_state` (an 832-byte XSAVE image, 64-aligned: x87 + SSE + AVX; FXSAVE in its first 512 bytes on a CPU without AVX) is saved and restored at every switch point, together with `fs_base`:
  - save and restore: `switch_to_next`, `block_current`, `stop_and_switch_tf`;
  - restore only: `kill_and_switch_tf`, `start_first`.
- Starting FPU state:
  - `fpu::init_this_cpu` (every CPU, from `cpu::init_this_cpu`) enables SSE and, if CPUID has XSAVE + AVX, CR4.OSXSAVE and XCR0 = x87|SSE|AVX (nothing wider: the target is Zen 3). Without that every VEX instruction in user code is #UD.
  - `fpu::init()` captures a clean template. It must run before the first `Process` exists.
  - fork: a fresh `fpu::save()` of the live registers, plus the parent's FS base read from the live MSR.
  - clone: the template.
  - exec: the template, written straight to the hardware.
- **Signal frames carry the XSAVE image** (64-aligned frame). `sigreturn` passes it through `fpu::sanitize` first, which clears MXCSR bits outside the CPU's `MXCSR_MASK` and resets the XSAVE header (XSTATE_BV ⊆ XCR0, XCOMP_BV and reserved bytes zero): the frame is user memory, and either one bad makes `xrstor` #GP in the kernel.
- Tests: `fpu_test`, `sse_test` (E–G: ymm across preemption and signals, a garbage XSAVE header), hw_test `init_this_cpu_restores_what_an_ap_lacks` (XCR0/OSXSAVE).
- TSS: one per CPU (RSP0 + double-fault IST stack). See `smp-interrupts-time.md`.

## SMP safety rules of the scheduler

- **`LEAVING[cpu]`** = the kernel stack a CPU has switched away from but is still executing on.
  - Set by `note_leaving` on every switch; cleared by the asm right after `mov rsp, <new frame>`. The timer/IPI stubs return a `Resume` in RAX:RDX for this; `jump_to_trapframe` passes `leaving_slot()`.
  - No other CPU picks that process or frees that stack.
- **Every kernel stack is freed through `pending_stack_frees`**, including `waitpid`'s reap: the zombie's `sys_exit` may still be unwinding on another CPU.
- **Never drop the last `Arc<AddressSpace>` of a table some CPU has in CR3.** Its PML4 would go back to the buddy and could be reused at once.
  - `sys_exec` drops the old space only after `activate()`.
  - `kill_current` parks a dying thread's space in `retiring[cpu]` until the next one is loaded.

## Blocking and wakeups

- **Check-then-sleep must be one step**, because a waker on another CPU can run between "register" and "block". Mechanisms:
  - `poll`, `epoll_wait`, stdin: register *and* block under the scheduler lock, re-check readiness after registering, and restart the syscall (`rip -= 2`) if something slipped in.
  - pipes and sockets (they register inside `FileHandle::read` and cannot hold the scheduler lock): wakers use `wake_or_defer`/`deliver_to_waiter`, which set `Process::wake_pending`; `block_current` consumes it instead of sleeping. Only valid for a waiter that registered on its way to blocking *and* whose wait the waker has claimed.
  - sockets additionally compare `unix::WAKE_EPOCH` read before the operation and after registering.
- Pipes keep a FIFO of waiters per end (`pipe_multi_test`), and look up the current pid *before* locking the buffer: `sys_fork` holds `SCHEDULER` while it dups pipes.
- `/proc/kdebug`: `early_wakes`.

## Interruptible waits (`process/wait.rs`, `sched::WaitCell`)

- A signal ends a blocked wait, as on Linux (`wait_intr_test`).
- **One-shot `Arc<WaitCell>` per wait**, shared by every registration of that wait:
  - A **waker must `claim` it before touching the waiter** (taking pipe bytes, writing `revents`, consuming a key). If the claim fails, it drops the entry.
  - A signal `cancel`s it (`Scheduler::interrupt_blocked`, called after every signal is queued).
  - Exactly one of the two wins. Stale entries are dropped when found.
  - Why a cell: registry locks (pipe, `POLL_WAITERS`, `SOCKETS`, `STDIN_WAITER`) come before `SCHEDULER`, so the signal path cannot clean registrations up itself.
- `block_current(tf, Wait)` names the wait: syscall number, return `rip`, `RestartPolicy`, and `Interruptible::{Cell, WaitPid, No}`.
  - The cell comes from `begin_wait` (under the scheduler lock) or `arm_wait` (pipes/sockets, after dropping their own lock).
  - A path that registered but does not sleep calls `abandon_wait`.
  - `block_current` refuses to sleep if an actionable signal is already pending.
- **EINTR vs restart is decided at delivery** (`signal::deliver_pending`, `sched::wait::restarts`):
  - a handler with `SA_RESTART` re-executes read/write/futex/waitpid/sockets/stdin (`rip = ret_rip - 2`, `rax = nr`);
  - `nanosleep` and `poll` return `EINTR` whenever a handler runs;
  - a stop, or no handler, re-executes the call.
- `sigsuspend`/`pause` have their own path (`in_sigsuspend`). A child's death calls `interrupt_blocked` *after* completing the parent's `waitpid`, so SIGCHLD never turns a completed wait into `EINTR`.
- `/proc/kdebug`: `waits_interrupted`.

## Process death

- `kill_current` (every death path) moves the fd table to `process::dead_files`, which is drained with no lock held at every syscall entry and in the idle loop. Never drop files under `SCHEDULER`: a socket's `Drop` takes it again.
- Children are reparented to PID 1 (`reparent_children`).
- A zombie whose blocked parent is woken for it is reaped right there (`reap_zombie`).
- Test: `lifecycle_test`.

## CPU time (`sched::cputime`, `sched::loadavg`)

- **Tick-sampled**: each tick on each CPU charges one tick to user (ring 3), idle (the idle process) or system (anything else in ring 0) (`Scheduler::tick(rsp, user_mode)`).
  - per CPU: `CPU_{USER,SYSTEM,IDLE}_TICKS` → `/proc/stat` `cpuN` lines;
  - per process: `Process::times`. A reap adds the child's times to the parent's `cutime`/`cstime` (`credit_reaped`, on every reap path).
  - 100 Hz = `USER_HZ`, so nothing is scaled.
- **`exec_ns` is measured**: started in `switch_in`, stopped in `note_leaving`. It backs the CPU-time clocks.
  - `CLOCK_THREAD_CPUTIME_ID` reads a per-CPU copy without taking the lock (taking it caused heavy contention).
  - `CLOCK_PROCESS_CPUTIME_ID` takes the lock and sums the thread group.
- A dying thread's time goes into its leader's `dead_threads`/`dead_threads_ns`, **not** into the leader's own fields.
- Load average: sampled on CPU 0 every 501 ticks; runnable = running + ready.
- Consumers: `/proc/stat`, `/proc/loadavg`, `times`, `getrusage`, `sysinfo`, BusyBox `top`/`ps`/`uptime`, `cpumon`.
- Test: `cputime_test`.

## Fault entries (`init/devices.rs`)

- #DE, #UD, #GP and #PF enter through `fault_entry!` asm stubs, not `x86-interrupt` shims: they build a full `TrapFrame` on the kernel stack (`xchg rax,[rsp]` turns the error-code slot into the `rax` slot; a fault without an error code gets a dummy first) and call `*_rust(tf, error_code)`. The pops and `iretq` resume whatever the frame holds, which is how a signal handler is run on the faulting context (`signal::deliver_fault`). A new stub must `cld`.
- Kernel-mode faults panic as before. `kill_current_user_process(reason, sig)` records the signal that killed the process (`SIGSEGV`, `SIGILL`, `SIGFPE`).

## Descriptor table

- `FileDescriptorTable` (`process/file.rs`) is two `Vec`s (handles, close-on-exec flags) that grow on demand to `MAX_FILES` = 256; a full table is `EMFILE`. **Never make it an inline array or put a per-fd array in a `BTreeMap` value**: at opt-level 0 those are copied by value several times and overflowed the boot stack (PID 1) and a kernel stack (`EPOLL_FD_MAP`, now keyed by `(pid, fd)`).
- `poll`/`epoll` snapshot the table into a boxed slice sized to the highest open fd (`open_extent`).
