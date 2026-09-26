// kernel/src/process/scheduler.rs
//
// Run-queue scheduler with time slices, priority aging, and wait queue.
//
// STRUCTURE:
//   run_queues[0..=10]  — ONLY Ready processes, indexed by effective_priority
//   wait_queue           — Blocked and Zombie processes (not scanned by scheduler)
//   running[cpu]         — what each CPU is executing (stage 7 of
//                          docs/smp/smp-plan.md: ONE lock, one core, one
//                          running slot per CPU — decision 1 of that plan)
//   idle[cpu]            — each CPU's own idle process (pid 0, as in Linux),
//                          never queued: a CPU runs it when nothing it may
//                          take is Ready, and no other CPU can pick it
//
// A process moves between these containers:
//   add_process()   → run_queues[eff_pri]
//   switch_to_next  → running ↔ run_queues  (Ready processes only)
//   block_current() → running → wait_queue  (future: I/O wait)
//   wake(pid)       → wait_queue → run_queues[eff_pri]  (future: I/O complete)
//   kill_current()  → running → wait_queue as Zombie  (segfault, sys_exit)
//
// TIME SLICES + AGING:
//   Each process gets quantum = BASE_QUANTUM + eff_pri * BONUS ticks.
//   When exhausted: preempt, decay eff_pri by 1.
//   Every AGING_EPOCH ticks: boost waiting processes' eff_pri toward base.
//
// HISTORY:
//   - Removed IretFrame and kill_and_switch().  Replaced with
//     kill_and_switch_tf() which returns a *const TrapFrame, enabling
//     a FULL context switch (all GPRs restored) via jump_to_trapframe.
//     The old approach only overwrote the 5-field exception stack frame,
//     leaking RAX..R15 from the killed process into the next one.

use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Thin wrapper around `spin::MutexGuard<Scheduler>` that (1) reports every
/// acquire/release through `debug::SCHEDULER_LOCK` — permanent, always-on
/// diagnostics, see that type's doc comment — and (2) asserts on `drop`
/// that interrupts are still disabled.
///
/// That second part is the actual fix for the bug class this guard was
/// built to catch, not just observe: `sys_close`/`sys_dup2` used to call
/// `sti` one statement too early, while the scheduler guard from the same
/// block was still alive, so a timer tick landing in that reopened window
/// found SCHEDULER held forever. Rather than trying to *automatically
/// correct* every one of the ~90 call sites that pair a manual `cli`/`sti`
/// around `local_scheduler()` (which would need a global IRQ-nesting
/// counter with careful, error-prone special-casing at every context-
/// switch/iretq boundary — timer ISR, page-fault/GPF/etc. process-killing
/// paths, `jump_to_user`, `start_first_process` — a wrong reset there
/// would silently corrupt interrupt state kernel-wide, a *worse* bug than
/// the one being fixed), this instead makes the invariant those call
/// sites are already supposed to uphold ("interrupts stay off for
/// SCHEDULER's entire *real* lifetime") self-enforcing and impossible to
/// violate silently: the check is purely observational (reads RFLAGS,
/// changes no control flow), so it requires editing none of them, and it
/// fires deterministically the very first time the bad ordering executes
/// — in any normal test run — instead of needing an hours-long, timing-
/// dependent stress test to manifest as a hang. See `local_scheduler()`'s
/// matching acquire-time assertion for the other half (missing `cli`
/// before the call).
pub struct TrackedSchedulerGuard(Option<spin::MutexGuard<'static, Scheduler>>);

impl core::ops::Deref for TrackedSchedulerGuard {
    type Target = Scheduler;
    fn deref(&self) -> &Scheduler { self.0.as_ref().unwrap() }
}

impl core::ops::DerefMut for TrackedSchedulerGuard {
    fn deref_mut(&mut self) -> &mut Scheduler { self.0.as_mut().unwrap() }
}

impl Drop for TrackedSchedulerGuard {
    fn drop(&mut self) {
        self.0 = None;
        crate::debug::SCHEDULER_LOCK.record_release();
        assert!(
            !x86_64::instructions::interrupts::are_enabled(),
            "SCHEDULER guard dropped with interrupts already enabled (IF=1) — \
             a `sti` ran before this guard's scope actually closed. This is \
             exactly the bug class that caused a real, hours-to-diagnose \
             kernel deadlock (timer ISR spinning on `local_scheduler()` \
             forever) — see the `deadlock_scheduler_filehandle_drop` \
             session memory / this type's doc comment. Fix: don't bind the \
             guard to a name in a block that also calls `sti` — use it as a \
             bare temporary (drops at the end of its own statement) or \
             nest it in its own tighter sub-block that closes before `sti`."
        );
    }
}

// ── FS.base save / restore helpers ──────────────────────────────────────────
// FS.base (MSR 0xC000_0100) is used by mlibc for TLS.  We must save it
// when context-switching away from a process and restore it for the next one.

const IA32_FS_BASE: u32 = 0xC000_0100;

/// `shell` (`init::processes`), which adopts orphans — see `reparent_children`.
const INIT_PID: usize = 1;

#[inline(always)]
pub(crate) fn read_fs_base() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") IA32_FS_BASE,
            out("eax") lo,
            out("edx") hi,
            options(nostack, preserves_flags),
        );
    }
    (hi as u64) << 32 | lo as u64
}

#[inline(always)]
fn write_fs_base(val: u64) {
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") IA32_FS_BASE,
            in("eax") (val & 0xFFFF_FFFF) as u32,
            in("edx") (val >> 32) as u32,
            options(nostack, preserves_flags),
        );
    }
}
use crate::sync::Mutex;
use x86_64::VirtAddr;
use super::{Process, Pid, ProcessState, TrapFrame};
use crate::memory::address_space::AddressSpace;
use crate::memory::vma::Vma;

// ============================================================================
// TrapFrame SAVE/RESUME sequence tracking
// ============================================================================
//
// See `Process::tf_seq`/`tf_awaiting_resume`/`tf_last_resumed_seq`'s doc
// comments. Every place that overwrites a process's `trapframe` Box wholesale
// from a live register snapshot (a "SAVE") must call `tf_note_save`; every
// place that hands that Box's contents to `jump_to_trapframe`/`jump_to_user`
// (a "RESUME") must call `tf_note_resume`. Together they catch two shapes of
// "rewind":
//   1. A SAVE that clobbers a previous SAVE's content before any RESUME
//      ever consumed it (saved twice with no RESUME in between).
//   2. A RESUME that reads back a sequence number no greater than the last
//      one this same process actually resumed (stale/rewound content).
// Both call `debug::tf_record(...)`, which prints immediately and
// unconditionally (a real rewind should never happen, so the print-cost
// concern that keeps `ktrace!` gated doesn't apply here) plus keeps the last
// occurrence around for `/proc/kdebug` and the panic snapshot. This is the
// permanent net under the 2026-08-05 DF bug, whose whole signature was a
// process resumed from a frame older than its last save.

/// Call immediately before (or as part of) a full `*proc.trapframe = *tf`
/// copy. `site` names the call site (`"switch_to_next"`, `"block_current"`,
/// ...) for the printed/rendered diagnostic.
pub(crate) fn tf_note_save(proc: &mut Process, site: &'static str) {
    if proc.tf_awaiting_resume {
        crate::debug::tf_record(proc.pid.0 as u64, site, proc.tf_seq, proc.tf_seq + 1);
    }
    proc.tf_seq = proc.tf_seq.wrapping_add(1);
    proc.tf_awaiting_resume = true;
}

/// Call immediately before handing `&*proc.trapframe` to
/// `jump_to_trapframe`/`jump_to_user` (including indirectly, via returning
/// the pointer up to a caller that will). `site` names the call site.
pub(crate) fn tf_note_resume(proc: &mut Process, site: &'static str) {
    if let Some(prev) = proc.tf_last_resumed_seq {
        if proc.tf_seq <= prev {
            crate::debug::tf_record(proc.pid.0 as u64, site, prev, proc.tf_seq);
        }
    }
    proc.tf_last_resumed_seq = Some(proc.tf_seq);
    proc.tf_awaiting_resume = false;
}

// ============================================================================
// Per-CPU fast-path pointers (updated on every context switch, IF=0)
// ============================================================================
//
// These let the page fault handler look up the running process's AddressSpace
// and PID without acquiring the SCHEDULERS Mutex.
//
// Safety invariant: a fault handler always runs with IF=0 on a single CPU.
// Between a context switch updating these atomics and the next switch, no
// other context can run on the same CPU, so the pointer is always valid.

static CURRENT_AS_PTR: [AtomicUsize; crate::cpu::MAX_CPUS] =
    [const { AtomicUsize::new(0) }; crate::cpu::MAX_CPUS];
static CURRENT_PID_FAST: [AtomicUsize; crate::cpu::MAX_CPUS] =
    [const { AtomicUsize::new(0) }; crate::cpu::MAX_CPUS];

/// Re-sync the per-CPU fast-path pointers for the already-running process.
///
/// Needed after anything replaces `proc.address_space` with a new `Arc`
/// in place (e.g. `sys_exec`'s image swap) — the page fault handler's
/// `find_vma_fast`/`current_as_fast` read a cached `Arc::as_ptr` that a plain
/// field assignment does not update, so without this call every fault in the
/// new address space would look up VMAs in the old (dropped or otherwise
/// unrelated) one and spuriously report "no VMA".
pub fn refresh_current_fast(proc: &Process) {
    update_current_fast(proc);
}

/// Update the per-CPU fast-path pointers to reflect `proc` as the running process.
/// Called with interrupts disabled, just before storing into `self.running`.
#[inline]
fn update_current_fast(proc: &Process) {
    let cpu = crate::cpu::cpu_id();
    // Arc::as_ptr gives a stable pointer to the shared AddressSpace's heap
    // allocation — valid as long as *any* Arc reference is alive, which
    // `proc.address_space` itself guarantees for as long as `proc` is the
    // running process on this CPU.
    CURRENT_AS_PTR[cpu].store(
        alloc::sync::Arc::as_ptr(&proc.address_space) as usize,
        Ordering::Release,
    );
    CURRENT_PID_FAST[cpu].store(proc.pid.0, Ordering::Release);
}

/// Clear the per-CPU fast-path pointers (no process running on this CPU).
#[inline]
fn clear_current_fast() {
    let cpu = crate::cpu::cpu_id();
    CURRENT_AS_PTR[cpu].store(0, Ordering::Release);
    CURRENT_PID_FAST[cpu].store(0, Ordering::Release);
}

// The tick-accounting constants (`BASE_QUANTUM`, `PRIORITY_QUANTUM_BONUS`,
// `AGING_EPOCH`) and their arithmetic (`quantum_for`) now live entirely
// behind `SchedCore::start_slice`/`advance_ticks`/`consume_quantum` — see
// `docs/sched/sched-extraction-plan.md` step 4 — so none of them need to be
// imported here anymore.

// ============================================================================
// Tick source for SchedCore::advance_ticks (docs/prompt-sched-bugs.md's
// injectable-clock step)
// ============================================================================
//
// `SchedCore` no longer owns a tick counter itself — `advance_ticks` now
// takes a `&impl sched::Clock` and only remembers *where* the last aging
// epoch was declared (see that method's doc comment in `sched/src/core.rs`).
// Something has to own the counter it used to own internally, and that
// something is here, in the adapter, not in `sched`, for the same reason
// `TrapFrame`/`fxsave`/CR3 stay here: it's real hardware-adjacent state, not
// plain data the host-testable core can hold.
//
// One counter per CPU slot, but only `TICKS[0]` moves since stage 7 of
// docs/smp/smp-plan.md: there is one `SchedCore` for every CPU now, and its
// aging epoch is global work, driven by CPU 0's tick alone (see `tick`).
static TICKS: [AtomicU64; crate::cpu::MAX_CPUS] =
    [const { AtomicU64::new(0) }; crate::cpu::MAX_CPUS];

/// Adapts `TICKS` to `sched::Clock`.
///
/// This is deliberately option (a) from the two considered for this step —
/// a plain kernel-owned counter, incremented once per `Scheduler::tick()`
/// call, reproducing `global_ticks`'s exact "one call, one tick" cadence —
/// and NOT option (b), hooking the real clocksource `kernel::time`
/// already has (TSC-backed, jiffies fallback — see CLAUDE.md's "Time
/// Subsystem" section). (b) is more honest about what "50 ticks" should
/// mean (wall-clock time instead of "50 timer-ISR firings, however long
/// those actually took"), but it changes the aging cadence the moment a
/// tick is ever coalesced, delayed, or skipped relative to real time — and
/// this step's whole point is proving the clock-injection refactor changes
/// nothing about production behavior. (a) has zero behavior-change risk by
/// construction: it counts exactly what `global_ticks` used to count, just
/// one call frame further out. (b) is left as a documented, deliberately
/// NOT taken next step, same as the crate-level doc comment's "Known
/// limitations" section already does for the aging bug this crate
/// inherited unfixed.
struct KernelClock;

impl sched::Clock for KernelClock {
    fn now_ticks(&self) -> u64 {
        TICKS[0].load(Ordering::Relaxed)
    }
}

const MAX_CPUS: usize = crate::cpu::MAX_CPUS;
const _: () = assert!(sched::MAX_CPUS == crate::cpu::MAX_CPUS);

/// The one scheduler (stage 7 of `docs/smp/smp-plan.md`, decision 1): every
/// CPU schedules from the same run queues under the same lock, so `kill`,
/// `waitpid`, `all_pids` and every wakeup keep their single-CPU shape.
static SCHEDULER: Mutex<Scheduler> = Mutex::new(Scheduler::new());

/// The kernel stack each CPU has switched *away* from but is still
/// executing on, or 0 — Linux's `on_cpu`, kept per CPU rather than per
/// process so it survives the process itself being dropped (a thread's
/// exit). Set under the scheduler lock by every switch whose outgoing
/// process owns the stack the CPU is on (`note_leaving`); cleared by the
/// asm that finally leaves it — `jump_to_trapframe_raw` and the timer/IPI
/// stubs, right after `mov rsp, <new frame>`.
///
/// Two things wait on it. **Picking:** a process whose stack another CPU is
/// still on is skipped (`eligible`) — resuming it would run its syscalls on
/// that same stack under the other CPU's feet. **Freeing:** a queued kernel
/// stack is freed only once no CPU is on it (`Scheduler::tick`), which is
/// what `waitpid` reaping a zombie whose exit is still unwinding on another
/// CPU needs.
static LEAVING: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// CPUs whose running slot is live (they have entered the scheduler).
static SCHEDULING: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// Where this CPU's `LEAVING` slot lives, for the asm that clears it.
pub fn leaving_slot() -> *mut u64 {
    LEAVING[crate::cpu::cpu_id()].as_ptr()
}

/// Is `cpu` scheduling processes?
pub fn is_scheduling(cpu: usize) -> bool {
    cpu < MAX_CPUS && SCHEDULING[cpu].load(Ordering::Acquire)
}

/// Is `cpu` running its idle process, or not scheduling at all (an inert
/// AP)? For picking self-test readers (`tlb_selftest::run`). IF=0.
pub fn cpu_is_idle(cpu: usize) -> bool {
    if !is_scheduling(cpu) {
        return true;
    }
    local_scheduler().running[cpu].as_ref().map_or(true, |p| p.pid.0 == 0)
}

/// May *this* CPU take `p`? Not while another CPU is still on its stack.
/// This CPU's own `LEAVING` is no obstacle: resuming the process whose
/// stack it is on is exactly the single-CPU case.
fn eligible(me: usize, p: &Process) -> bool {
    let top = p.kernel_stack.as_u64();
    !(0..MAX_CPUS).any(|c| c != me && LEAVING[c].load(Ordering::Acquire) == top)
}

/// Is any CPU still on the kernel stack whose top is `top`?
fn stack_in_use(top: u64) -> bool {
    LEAVING.iter().any(|l| l.load(Ordering::Acquire) == top)
}

/// Record that this CPU is switching away from `proc`, if the stack it is
/// executing on is `proc`'s (see `LEAVING`), and stop its run clock
/// (`Process::exec_ns`). Every switch away from a process passes here, as
/// every switch to one passes through `switch_in`, which starts the clock.
fn note_leaving(proc: &mut Process) {
    let now = crate::time::ktime_get();
    proc.exec_ns += now.saturating_sub(proc.run_since_ns);
    proc.run_since_ns = now;

    let rsp: u64;
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags)) };
    let top = proc.kernel_stack.as_u64();
    let lo = top - (1u64 << crate::init::processes::KERNEL_STACK_ORDER);
    if (lo..top).contains(&rsp) {
        LEAVING[crate::cpu::cpu_id()].store(top, Ordering::Release);
    }
}

// ── Per-CPU scheduling counters (`sched:` in /proc/kdebug) ──────────────
static CPU_SWITCHES: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
// Where each CPU's ticks went (`sched::cputime::classify`): the source of
// `/proc/stat`'s `cpuN` lines.
static CPU_USER_TICKS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static CPU_SYSTEM_TICKS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static CPU_IDLE_TICKS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// The 1/5/15-minute load averages, `sched::loadavg` fixed point, written
/// by CPU 0's tick under the scheduler lock.
static LOADAVG: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static RESCHED_IPIS: AtomicU64 = AtomicU64::new(0);
/// The most CPUs ever running non-idle processes at once.
static MAX_CONCURRENT: AtomicU64 = AtomicU64::new(0);
/// The most CPUs ever running processes of one address space at once —
/// 2 or more means threads of one process really ran in parallel.
static MAX_SAME_AS: AtomicU64 = AtomicU64::new(0);
/// Picks that skipped a Ready process because another CPU was still on
/// its kernel stack.
static LEAVING_SKIPS: AtomicU64 = AtomicU64::new(0);

/// The vector of the reschedule IPI: "you are idle and there is work".
pub const RESCHED_VECTOR: u8 = 0xF2;

/// Acquires the scheduler lock.
/// CALLER must disable interrupts before calling (cli) and
/// re-enable after dropping the guard (sti) — enforced by assertion, not
/// just this doc comment: see `TrackedSchedulerGuard::drop`'s doc comment
/// for why an assertion here (missing `cli`) instead of an automatic fix.
///
/// The name predates stage 7, when there was one scheduler per CPU; there
/// is one for all of them now.
#[track_caller]
pub fn local_scheduler() -> TrackedSchedulerGuard {
    assert!(
        !x86_64::instructions::interrupts::are_enabled(),
        "local_scheduler() called with interrupts enabled (IF=1) — the \
         caller must `cli` first. Same bug class `TrackedSchedulerGuard`'s \
         drop-time assertion catches on the other end; see its doc comment."
    );
    let guard = SCHEDULER.lock();
    crate::debug::SCHEDULER_LOCK.record_acquire(core::panic::Location::caller());
    TrackedSchedulerGuard(Some(guard))
}

pub struct Scheduler {
    /// Per-priority run queues (ONLY Ready processes), the wait queue
    /// (Blocked and Zombie processes, not scanned during scheduling), the
    /// monotonic PID counter (0 is reserved for idle), and the tick
    /// accounting (remaining ticks in the current slice, global tick
    /// counter for aging epochs) — moved into the host-testable `sched`
    /// crate's generic core. See `docs/sched/sched-extraction-plan.md`.
    core: sched::SchedCore<Process>,

    /// What each CPU is executing, indexed by `cpu::cpu_id()`.
    running: [Option<Box<Process>>; MAX_CPUS],

    /// Each CPU's idle process while it is not running (see the module
    /// comment).
    idle: [Option<Box<Process>>; MAX_CPUS],

    /// Kernel stacks awaiting `phys_free` — populated by `kill_current`'s
    /// thread-reap path, which runs *on the dying thread's own kernel
    /// stack* (called mid-syscall/exception, before the switch-away has
    /// actually happened via `jump_to_trapframe`/`iretq`). Freeing those
    /// physical frames immediately would let the Buddy allocator hand them
    /// out to something else while this CPU is still executing on them.
    /// Drained by `tick()` instead, which only ever runs once we're
    /// guaranteed to be on a different process's stack (interrupts stay
    /// off, hence no nested `tick()`, from the moment `kill_current` runs
    /// until the new process's `iretq` re-enables them).
    pending_stack_frees: Vec<VirtAddr>,

    /// Same deferral, for a dying thread's `owned_stack_vma` (its mlibc
    /// `mmap()`-allocated user-mode stack — see `Process::owned_stack_vma`).
    /// The `AddressSpace` is kept alive via this `Arc` for as long as the
    /// entry is queued, even if the `Process` that referenced it has
    /// already been dropped — it may otherwise be the last reference if the
    /// thread's parent process has also exited.
    pending_vma_frees: Vec<(alloc::sync::Arc<AddressSpace>, u64, usize)>,

    /// An address space a CPU may still have loaded in CR3 while its last
    /// owner goes away — a thread reaped by `kill_current`, possibly the
    /// last holder of its process's space. Dropped by `switch_in` once that
    /// CPU has loaded the next process's table: a PML4 freed while still
    /// loaded is a frame another CPU can reuse under this one's feet.
    retiring: [Option<alloc::sync::Arc<AddressSpace>>; MAX_CPUS],
}

impl Scheduler {
    pub const fn new() -> Self {
        Self {
            core: sched::SchedCore::new(),
            running: [const { None }; MAX_CPUS],
            idle: [const { None }; MAX_CPUS],
            pending_stack_frees: Vec::new(),
            pending_vma_frees: Vec::new(),
            retiring: [const { None }; MAX_CPUS],
        }
    }

    // ====================================================================
    // PID management
    // ====================================================================

    pub fn allocate_pid(&mut self) -> Pid {
        Pid(self.core.allocate_pid())
    }

    // ====================================================================
    // Process insertion
    // ====================================================================

    pub fn add_process(&mut self, process: Box<Process>) {
        let (pid, base) = (process.pid.0, process.priority);
        let pri = self.core.add_reset_to_base(process);
        // `add_reset_to_base` just set `effective_priority = base_priority`,
        // so the third logged value (the process's now-current effective
        // priority) is by definition equal to `base` here — this print
        // used to run *before* the push (reading `process.effective_priority`
        // directly); it now runs after, with the same values, since the push
        // itself has no observable output of its own.
        crate::serial_println!(
            "Scheduler: Added PID {} (base pri {}, effective {}) to queue[{}]",
            pid, base, base, pri
        );
        self.kick_idle(false);
    }

    /// Free a dead process's kernel stack once no CPU is on it — see
    /// `pending_stack_frees` and `LEAVING`.
    pub fn defer_stack_free(&mut self, stack_top: VirtAddr) {
        self.pending_stack_frees.push(stack_top);
    }

    /// Give `cpu` its idle process (pid 0). Never queued: see `idle`.
    pub fn add_idle(&mut self, cpu: usize, mut idle: Box<Process>) {
        idle.state = ProcessState::Ready;
        self.idle[cpu] = Some(idle);
    }

    /// The blocked/zombie/stopped queue. This used to be a `pub` field; it is
    /// now reached through the core so that `run_queues` — which the core keeps
    /// private — cannot be reached from outside at all. That is what makes the
    /// "queue index == queue_index(effective priority)" invariant enforceable
    /// instead of a convention. See docs/sched/sched-extraction-plan.md,
    /// decision 1.
    pub fn wait_queue(&self) -> &VecDeque<Box<Process>> {
        self.core.wait_queue()
    }

    /// Mutable counterpart of [`Self::wait_queue`].
    pub fn wait_queue_mut(&mut self) -> &mut VecDeque<Box<Process>> {
        self.core.wait_queue_mut()
    }

    // ====================================================================
    // Current process access — O(1)
    // ====================================================================

    pub fn current_pid(&self) -> Option<Pid> {
        self.running_ref().map(|p| p.pid)
    }

    /// The process running on *this* CPU.
    pub fn running_ref(&self) -> Option<&Process> {
        self.running[crate::cpu::cpu_id()].as_deref()
    }

    /// The process running on *this* CPU.
    pub fn running_mut(&mut self) -> Option<&mut Process> {
        self.running[crate::cpu::cpu_id()].as_deref_mut()
    }

    /// Non-idle processes running on any CPU.
    fn iter_running(&self) -> impl Iterator<Item = &Process> + '_ {
        self.running.iter().filter_map(|r| r.as_deref()).filter(|p| p.pid.0 != 0)
    }

    fn iter_running_mut(&mut self) -> impl Iterator<Item = &mut Process> + '_ {
        self.running.iter_mut().filter_map(|r| r.as_deref_mut()).filter(|p| p.pid.0 != 0)
    }

    /// `sched`'s invariants over the core *and* every CPU's running slot:
    /// no process on two CPUs, or on one and queued. Cheap enough for
    /// `/proc/kdebug`, which reports it.
    pub fn check_invariants(&self) -> Result<(), sched::invariants::Violation> {
        self.core.check_invariants_with_running(self.running.iter().filter_map(|r| r.as_deref()))
    }

    // ====================================================================
    // Iteration (debug / introspection)
    // ====================================================================

    /// Iterate over ALL processes: running on any CPU + run queues + wait
    /// queue. The idle processes are not listed (pid 0, one per CPU — Linux
    /// does not list its idle tasks in `/proc` either).
    pub fn iter_all(&self) -> impl Iterator<Item = &Process> + '_ {
        self.iter_running().chain(self.core.iter_queued())
    }

    // ====================================================================
    // CPU time (`sched::cputime`)
    // ====================================================================

    /// Any non-idle process, wherever it is: running on some CPU or queued.
    fn process_anywhere_mut(&mut self, pid: Pid) -> Option<&mut Process> {
        self.running.iter_mut()
            .filter_map(|r| r.as_deref_mut())
            .filter(|p| p.pid.0 != 0)
            .chain(self.core.iter_queued_mut())
            .find(|p| p.pid == pid)
    }

    /// `child` has just been reaped: its time, and what it had collected
    /// from its own children, becomes its parent's `cutime`/`cstime`
    /// (`ProcTimes::reap`). Every reap path calls this — `waitpid`'s and
    /// `reap_zombie`.
    pub fn credit_reaped(&mut self, child: &Process) {
        let Some(ppid) = child.parent_pid else { return };
        let times = child.times;
        if let Some(parent) = self.process_anywhere_mut(ppid) {
            parent.times.reap(&times);
        }
    }

    /// A thread has exited: its time (and what its own dead threads had
    /// left it) goes to its thread group's leader — the process that
    /// shares its address space and is not a thread — as
    /// `dead_threads`, so the group's clocks keep it without the leader's
    /// own thread clock gaining it (Linux keeps it in `signal->utime`).
    /// Lost if the leader is gone or has exec'd, as the thread's own record
    /// is about to be.
    fn fold_into_leader(&mut self, thread: &Process) {
        let mut times = thread.times;
        times.absorb_thread(&thread.dead_threads);
        let ns = thread.exec_ns + thread.dead_threads_ns;
        let space = &thread.address_space;
        let leader = self.running.iter_mut()
            .filter_map(|r| r.as_deref_mut())
            .filter(|p| p.pid.0 != 0)
            .chain(self.core.iter_queued_mut())
            .find(|p| !p.is_thread && Arc::ptr_eq(&p.address_space, space));
        if let Some(leader) = leader {
            leader.dead_threads.absorb_thread(&times);
            leader.dead_threads_ns += ns;
        }
    }

    /// The running process's CPU time, its own and its thread group's
    /// (every process sharing its address space — this kernel has no
    /// thread-group id; threads are processes that share one). IF=0.
    pub fn current_cpu_times(&self) -> Option<CpuTimesOf> {
        let me = self.running_ref()?;
        let now = crate::time::ktime_get();
        let exec_now = |p: &Process| {
            let running = self.running.iter().any(|r| r.as_deref().map_or(false, |r| r.pid == p.pid));
            p.exec_ns + if running { now.saturating_sub(p.run_since_ns) } else { 0 }
        };
        let mut out = CpuTimesOf {
            own: me.times,
            own_exec_ns: exec_now(me),
            group: sched::cputime::ProcTimes::default(),
            group_exec_ns: 0,
        };
        for p in self.iter_all().filter(|p| Arc::ptr_eq(&p.address_space, &me.address_space)) {
            out.group.absorb_thread(&p.times);
            out.group.absorb_thread(&p.dead_threads);
            out.group_exec_ns += exec_now(p) + p.dead_threads_ns;
        }
        Some(out)
    }

    /// Check the currently-`running` process's pending signals against `tf`
    /// (must point at that same process's live TrapFrame — see callers)
    /// and act on the outcome: a caught signal redirects `tf` in place and
    /// is returned unchanged; an uncaught default-terminate signal kills
    /// the process via `kill_and_switch_tf` and repeats against whatever
    /// gets scheduled next, so the final returned pointer always belongs to
    /// a process that's either signal-clean or non-existent-and-replaced.
    ///
    /// Centralizes the same three-line loop that both `trapframe::
    /// jump_to_user` and `syscall_handler_asm`'s tail need — the latter
    /// has no `jump_to_trapframe` call of its own to hang the check off of,
    /// so it calls this directly instead of going through `jump_to_user`.
    pub fn resolve_signals(&mut self, mut tf: *const TrapFrame) -> *const TrapFrame {
        // Ring-3 code segment selector (see Process::new_user's trapframe.cs).
        const USER_CS: u64 = 0x23;
        loop {
            // Only attempt delivery when `tf` genuinely represents a
            // user-mode return point. A kernel-mode-interrupted trapframe's
            // `rsp` is a *kernel* stack address, not a user one — treating
            // it as user (as signal delivery must, to push a handler frame)
            // would corrupt whatever that kernel rsp actually pointed at.
            // Pending signals just stay pending and get retried the next
            // time this process genuinely returns to user mode. Found via
            // `sys_close` briefly running with interrupts enabled and no
            // held lock, letting a timer tick preempt mid-syscall and save
            // exactly this kind of kernel-mode trapframe (now fixed there
            // too, but this check is what makes the class of mistake safe
            // wherever else it might still be lurking).
            if unsafe { (*tf).cs } != USER_CS {
                return tf;
            }

            let outcome = match self.running_mut() {
                Some(proc) if proc.privilege == crate::process::PrivilegeLevel::User => {
                    super::signal::deliver_pending(proc, tf as *mut TrapFrame)
                }
                _ => super::signal::SignalOutcome::None,
            };

            match outcome {
                super::signal::SignalOutcome::Terminate(sig) => {
                    // Tag the about-to-die process with the signal that
                    // killed it (read back by `Process::wait_status_word()`)
                    // and capture what its parent needs to be told, before
                    // `kill_and_switch_tf` below takes it out of `self.running`.
                    let (dead_pid, parent_pid) = match self.running_mut() {
                        Some(proc) => {
                            proc.killed_by_signal = Some(sig);
                            let parent = if proc.is_thread { None } else { proc.parent_pid };
                            (proc.pid.0, parent)
                        }
                        None => (0, None),
                    };

                    tf = self.kill_and_switch_tf("uncaught signal");
                    self.notify_child_death(dead_pid, parent_pid);
                    // Same side-table cleanup `sys_exit` does for a normal
                    // exit — see `syscall::cancel_all_waiters`'s doc comment
                    // for why skipping this here specifically caused a
                    // stale poll waiter to leak and later spuriously affect
                    // an unrelated process.
                    super::syscall::cancel_all_waiters(dead_pid);
                }
                super::signal::SignalOutcome::Stop(sig) => {
                    // Same shape as Terminate above, but parks the process
                    // as Stopped instead of discarding it — see
                    // `stop_and_switch_tf`/`notify_child_stopped`.
                    let (stopped_pid, parent_pid) = match self.running_mut() {
                        Some(proc) => {
                            proc.stopped_by_signal = Some(sig);
                            proc.stop_reported = false;
                            (proc.pid.0, proc.parent_pid)
                        }
                        None => (0, None),
                    };

                    tf = self.stop_and_switch_tf(tf);
                    self.notify_child_stopped(stopped_pid, parent_pid);
                }
                _ => return tf,
            }
        }
    }

    /// Find a Ready (run_queues) or Blocked/Zombie (wait_queue) process by
    /// pid — i.e. everything *except* the currently running one, which
    /// callers (e.g. `sys_kill`) handle separately via `running_mut()`.
    /// Used to deliver a signal to a process other than the caller itself.
    ///
    /// Since stage 7 it also finds a process running on *another* CPU —
    /// the parent a child's death must reach may be running right now.
    pub fn find_process_mut(&mut self, pid: usize) -> Option<&mut Process> {
        let me = crate::cpu::cpu_id();
        let on_other_cpu = self.running.iter()
            .enumerate()
            .position(|(c, r)| c != me && r.as_ref().map_or(false, |p| p.pid.0 == pid && pid != 0));
        match on_other_cpu {
            Some(c) => self.running[c].as_deref_mut(),
            None => self.core.find_mut(|p| p.pid.0 == pid),
        }
    }

    // ====================================================================
    // Kill current process (user segfault, sys_exit)
    // ====================================================================

    /// Mark the running process as Zombie and move it to the wait queue —
    /// unless it's a thread (`is_thread`), in which case it's reaped
    /// immediately instead (dropped here and now).
    ///
    /// Threads never get an explicit `waitpid()` call collecting them:
    /// mlibc's `pthread_join()` (upstream, shared by every sysdeps port —
    /// see `Process::is_thread`'s doc comment) is purely futex-based and
    /// never issues one. Zombie-parking a thread the normal way would leak
    /// its `Process` struct (and kernel stack) forever, since nothing will
    /// ever remove it from `wait_queue`. So this is the thread-exit
    /// equivalent of an implicit, always-successful `waitpid()`.
    ///
    /// Returns true if a process was killed, false if nothing was running.
    /// After calling this, the caller must trigger a context switch
    /// (the running slot is now empty).
    pub fn kill_current(&mut self, reason: &str) -> bool {
        let me = crate::cpu::cpu_id();
        if let Some(mut proc) = self.running[me].take() {
            assert!(proc.pid.0 != 0, "kill_current on an idle process");
            note_leaving(&mut proc);
            self.reparent_children(proc.pid);
            // Its files close now, as Linux's `do_exit` does — but not
            // here, under this lock: see `dead_files`. (`sys_exit` has
            // already swapped in an empty table; this moves that one.)
            let files = core::mem::replace(
                &mut proc.files,
                alloc::sync::Arc::new(crate::sync::Mutex::new(
                    crate::process::file::FileDescriptorTable::new(),
                )),
            );
            super::dead_files::defer(files);
            crate::serial_println!(
                "💀 Killed PID {} ({}): {}",
                proc.pid.0,
                core::str::from_utf8(&proc.name)
                    .unwrap_or("<?>")
                    .trim_end_matches('\0'),
                reason,
            );
            if proc.is_thread {
                crate::serial_println!("  → thread, reaped immediately (no waitpid() will ever collect it)");
                self.fold_into_leader(&proc);
                // Defer the kernel stack's phys_free — see pending_stack_frees'
                // doc comment for why it can't happen right here.
                self.pending_stack_frees.push(proc.kernel_stack);
                // Same deferral for the thread's own mmap'd user stack, if
                // sys_clone found one — see pending_vma_frees' doc comment.
                if let Some((start, size_pages)) = proc.owned_stack_vma {
                    self.pending_vma_frees.push((proc.address_space.clone(), start, size_pages));
                }
                // Its address space is still this CPU's CR3: see `retiring`.
                self.retiring[me] = Some(proc.address_space.clone());
                // `proc` drops here: releases the Process struct itself and its
                // Arc references to the shared AddressSpace/FileDescriptorTable
                // (safe immediately — unlike the kernel stack, that's ordinary
                // kernel-heap memory, not the stack this code is executing on).
            } else {
                proc.state = ProcessState::Zombie;
                self.core.park(proc);
            }
            true
        } else {
            false
        }
    }

    /// Kill the running process and schedule the next one.
    ///
    /// Returns a pointer to the next process's FULL TrapFrame (all GPRs
    /// + iret fields).  The caller must use `jump_to_trapframe` to load
    /// all registers and iretq into the new process.
    ///
    /// This replaces the old `kill_and_switch` which returned only the 5
    /// iret-frame fields, leaking GPR values from the killed process.
    ///
    /// Also activates the new address space and updates TSS.
    ///
    /// Panics if no Ready process exists (shouldn't happen with idle).
    pub fn kill_and_switch_tf(&mut self, reason: &str) -> *const TrapFrame {
        self.kill_current(reason);
        clear_current_fast();
        let next = self.pick_next();
        // `switch_in` also restores the next process's FS base: the live one
        // still belongs to the process just killed, and without it the next
        // one runs on the dead process's TLS pointer — 0 after a child that
        // never set one, which faulted `ash` in mlibc's `get_current_tcb`
        // the moment a script's external command exited (found by the first
        // autorun job).
        self.switch_in(next, "kill_and_switch_tf")
    }

    /// Stop the running process (job control: SIGSTOP/SIGTSTP) and schedule
    /// the next one. Mirrors `kill_and_switch_tf`, except the process is
    /// parked as `ProcessState::Stopped` in `wait_queue` instead of being
    /// discarded — `sys_kill`'s SIGCONT handling (`wake_stopped`) is the
    /// only thing that ever resumes it.
    ///
    /// Unlike `kill_and_switch_tf` (which never needs the outgoing process's
    /// register state, since it's being thrown away), this *does* need to
    /// save `tf` into `proc.trapframe` first — `tf` may be the live syscall-
    /// entry stack frame rather than `proc.trapframe` itself (see
    /// `resolve_signals`'s call sites), and a stopped process must resume
    /// later exactly where it left off.
    pub fn stop_and_switch_tf(&mut self, tf: *const TrapFrame) -> *const TrapFrame {
        let me = crate::cpu::cpu_id();
        if let Some(mut proc) = self.running[me].take() {
            tf_note_save(&mut proc, "stop_and_switch_tf");
            unsafe { *proc.trapframe = *tf; }
            proc.fs_base = read_fs_base();
            unsafe { super::fpu::save(&mut proc.fpu_state); }
            crate::serial_println!(
                "⏸ Stopped PID {} ({})",
                proc.pid.0,
                core::str::from_utf8(&proc.name).unwrap_or("<?>").trim_end_matches('\0'),
            );
            proc.state = ProcessState::Stopped;
            note_leaving(&mut proc);
            self.core.park(proc);
        }
        clear_current_fast();
        let next = self.pick_next();
        self.switch_in(next, "stop_and_switch_tf")
    }

    /// Queue `sig` on every process whose `pgid` matches — used for
    /// job-control signals (Ctrl-C/Ctrl-Z at the tty, see `tty::feed_input`)
    /// and `sys_kill`'s process-group target forms (`pid == 0` / negative).
    /// Caller must already hold the scheduler lock (this takes `&mut self`,
    /// not a fresh lock) — see `syscall::send_to_group` for the ISR-context
    /// wrapper that acquires one.
    pub fn queue_signal_to_group(&mut self, pgid: u32, sig: u32) {
        for proc in self.iter_running_mut() {
            if proc.pgid == pgid {
                super::signal::queue_signal(proc, sig);
            }
        }
        // `iter_queued_mut` yields run queues then wait queue, i.e. the same
        // order as the two separate loops (run queues, then wait queue) this
        // replaces.
        for proc in self.core.iter_queued_mut() {
            if proc.pgid == pgid {
                super::signal::queue_signal(proc, sig);
            }
        }
        self.interrupt_blocked();
    }

    /// Send `sig` to every process in group `pgid` — `kill(-pgid)`, and a
    /// terminal's signals (`crate::ipc::pty`). A `SIGCONT` or `SIGKILL`
    /// resumes the stopped ones first (`signal::resumes_stopped`).
    pub fn signal_group(&mut self, pgid: u32, sig: u32) {
        if super::signal::resumes_stopped(sig) {
            let stopped: Vec<usize> = self.core.wait_queue().iter()
                .filter(|p| p.pgid == pgid && matches!(p.state, ProcessState::Stopped))
                .map(|p| p.pid.0)
                .collect();
            for pid in stopped {
                self.wake_stopped(pid);
            }
        }
        self.queue_signal_to_group(pgid, sig);
    }

    /// Send `sig` to one process, `kill(pid)`'s way (see `signal_group`).
    pub fn signal_pid(&mut self, pid: usize, sig: u32) {
        if super::signal::resumes_stopped(sig) {
            self.wake_stopped(pid);
        }
        if self.current_pid().map(|p| p.0) == Some(pid) {
            if let Some(p) = self.running_mut() {
                super::signal::queue_signal(p, sig);
            }
        } else if let Some(p) = self.find_process_mut(pid) {
            super::signal::queue_signal(p, sig);
        }
        self.interrupt_blocked();
    }

    /// Every process whose controlling terminal is pty `index` loses it
    /// (the master closed: `crate::pty`).
    pub fn clear_ctty(&mut self, index: usize) {
        for p in self.iter_running_mut() {
            if p.ctty == Some(index) {
                p.ctty = None;
            }
        }
        for p in self.core.iter_queued_mut() {
            if p.ctty == Some(index) {
                p.ctty = None;
            }
        }
    }

    /// A process group `pgid` exists in session `sid`.
    pub fn group_in_session(&self, pgid: u32, sid: u32) -> bool {
        self.iter_all().any(|p| p.pgid == pgid && p.sid == sid && !matches!(p.state, ProcessState::Zombie))
    }

    /// POSIX's orphaned process group: no member has a parent in another
    /// group of the same session — nobody left who could continue it if
    /// it stopped, which is why a terminal answers `EIO` instead of
    /// stopping it (`tty::jobctl`).
    pub fn group_orphaned(&self, pgid: u32, sid: u32) -> bool {
        !self.iter_all()
            .filter(|p| p.pgid == pgid && !matches!(p.state, ProcessState::Zombie))
            .any(|p| {
                let Some(ppid) = p.parent_pid else { return false };
                self.iter_all().any(|q| q.pid == ppid && q.pgid != pgid && q.sid == sid)
            })
    }

    /// Wake every Blocked process a newly queued signal should interrupt.
    /// Every place that queues a signal calls this afterwards, under the
    /// same lock hold; `block_current` checks under this lock too
    /// (`interrupt_before_blocking`), so a signal sent from another CPU
    /// cannot fall between a check and a block.
    ///
    /// `rt_sigsuspend`/`pause` sleepers are woken as before (their `rax`
    /// was preset to `EINTR`). Any other wait is interrupted if it named
    /// itself interruptible (`process::wait`) and the signal wins its
    /// cell — a waker that already claimed the wait completes the call
    /// instead, and the signal is delivered after it, as on Linux. It
    /// used to be only the sigsuspend half: a signal to any other Blocked
    /// process was queued and waited for the natural wakeup, so a
    /// `SIGKILL` to a process in `sleep 100` took 100 s.
    pub fn interrupt_blocked(&mut self) {
        loop {
            let woke = self.core.wake_matching(
                |p| matches!(p.state, ProcessState::Blocked)
                    && p.in_sigsuspend
                    && super::signal::has_actionable(p),
                |p| {
                    p.in_sigsuspend = false;
                    p.state = ProcessState::Ready;
                },
            );
            if !woke {
                break;
            }
            self.kick_idle(false);
        }
        loop {
            // The cell is cancelled in the predicate: `wake_matching`
            // prepares exactly the entry it returned `true` for.
            let woke = self.core.wake_matching(
                |p| matches!(p.state, ProcessState::Blocked)
                    && !p.in_sigsuspend
                    && super::signal::has_actionable(p)
                    && p.wait.as_ref().is_some_and(try_interrupt),
                |p| {
                    finish_interrupt(p);
                    p.state = ProcessState::Ready;
                },
            );
            if !woke {
                break;
            }
            crate::debug::inc_waits_interrupted();
            self.kick_idle(false);
        }
    }

    /// Wake a Stopped process (SIGCONT): move it from `wait_queue` back to
    /// its run queue, exactly like `wake()` does for a Blocked one. Unlike
    /// `wake()`, this is the *only* wakeup path a Stopped process ever has
    /// — it can't wake itself the way a Blocked process does when its I/O
    /// completes, since being stopped isn't waiting on anything.
    pub fn wake_stopped(&mut self, pid: usize) -> bool {
        let woke = self.core.wake_matching(
            |p| p.pid.0 == pid && matches!(p.state, ProcessState::Stopped),
            |p| {
                p.state = ProcessState::Ready;
                p.stopped_by_signal = None;
            },
        );
        if woke {
            self.kick_idle(false);
        }
        woke
    }

    // ====================================================================
    // Blocking / wakeup (I/O wait)
    // ====================================================================

    /// Block the running process (copy TF into Box, move to wait_queue).
    ///
    /// Returns the next Ready process's TrapFrame pointer — or `current_tf`
    /// itself, unchanged but for `rax`, when a wakeup already arrived while
    /// this process was on its way here (`Process::wake_pending`): with
    /// another CPU as the waker, "register as a waiter, then block" is no
    /// longer one step, and a wakeup in between would otherwise be lost.
    pub fn block_current(&mut self, current_tf: *const TrapFrame, wait: super::wait::Wait) -> *const TrapFrame {
        let me = crate::cpu::cpu_id();
        if let Some(proc) = self.running[me].as_deref_mut() {
            if let Some(pending) = proc.wake_pending.take() {
                if let super::WakePending::Return(rax) = pending {
                    unsafe { (*(current_tf as *mut TrapFrame)).rax = rax; }
                }
                // Claimed by the waker that left this wakeup.
                proc.armed_wait = None;
                crate::debug::inc_early_wakes();
                return current_tf;
            }
            if interrupt_before_blocking(proc, wait) {
                crate::debug::inc_waits_interrupted();
                return current_tf;
            }
        }
        if let Some(mut proc) = self.running[me].take() {
            assert!(proc.pid.0 != 0, "the idle process blocked");
            tf_note_save(&mut proc, "block_current");
            unsafe { *proc.trapframe = *current_tf; }
            proc.fs_base = read_fs_base();
            unsafe { super::fpu::save(&mut proc.fpu_state); }
            let armed = proc.armed_wait.take();
            proc.wait = match wait.how {
                super::wait::Interruptible::No => {
                    // Nothing will ever claim a cell this wait did not use.
                    if let Some(c) = armed {
                        c.cancel();
                    }
                    None
                }
                super::wait::Interruptible::Cell(_) => Some(super::wait::ActiveWait { wait, cell: armed }),
                super::wait::Interruptible::WaitPid => {
                    if let Some(c) = armed {
                        c.cancel();
                    }
                    Some(super::wait::ActiveWait { wait, cell: None })
                }
            };
            proc.state = ProcessState::Blocked;
            note_leaving(&mut proc);
            self.core.park(proc);
        }
        // No process running on this CPU until we schedule the next one.
        clear_current_fast();
        let next = self.pick_next();
        self.switch_in(next, "block_current")
    }

    /// Arm a fresh wait cell on this CPU's running process and return it,
    /// for the caller to register wherever its waker looks (see
    /// `process::wait`). Must be followed by `block_current` with an
    /// `Interruptible::Cell` wait, or by `abandon_wait`.
    pub fn begin_wait(&mut self) -> alloc::sync::Arc<super::wait::WaitCell> {
        let cell = alloc::sync::Arc::new(super::wait::WaitCell::new());
        self.arm(cell.clone());
        cell
    }

    /// Make `cell` the running process's armed wait. A cell armed before
    /// and never slept in is cancelled, so what registered it goes stale.
    pub fn arm(&mut self, cell: alloc::sync::Arc<super::wait::WaitCell>) {
        if let Some(p) = self.running_mut() {
            if let Some(old) = p.armed_wait.replace(cell) {
                old.cancel();
            }
        }
    }

    /// The running process registered for a wait and is not going to sleep
    /// in it after all (it found what it wanted on a re-check).
    pub fn abandon_wait(&mut self) {
        if let Some(p) = self.running_mut() {
            if let Some(c) = p.armed_wait.take() {
                c.cancel();
            }
        }
    }

    /// Wake a Blocked process: move it from wait_queue to its run_queue.
    /// A process that is not Blocked is left alone (see `wake_or_defer` for
    /// the waker that must not lose a wakeup to a process still on its way
    /// to blocking).
    pub fn wake(&mut self, pid: usize) {
        let woke = self.core.wake_matching(
            |p| p.pid.0 == pid && matches!(p.state, ProcessState::Blocked),
            |p| {
                p.state = ProcessState::Ready;
            },
        );
        if woke {
            self.kick_idle(false);
        }
    }

    /// Wake a Blocked process and set its syscall return value in one scan.
    ///
    /// Combines what was previously two separate operations in the IPC delivery
    /// path (set trapframe.rax then call wake()) into a single wait_queue scan,
    /// halving the linear-search overhead for IPC hot paths.
    pub fn wake_with_retval(&mut self, pid: usize, rax: u64) {
        let woke = self.core.wake_matching(
            |p| p.pid.0 == pid && matches!(p.state, ProcessState::Blocked),
            |p| {
                p.trapframe.rax = rax;
                p.state = ProcessState::Ready;
            },
        );
        if woke {
            self.kick_idle(false);
        }
    }

    /// `wake`, for a waker whose waiter may not have blocked yet: its
    /// registration and its `block_current` are two steps, and another CPU
    /// can run the whole wakeup in between. Then the wakeup is left in
    /// `Process::wake_pending` for that `block_current` to consume.
    ///
    /// Only for waiters registered on the way to blocking and removed by the
    /// wakeup itself (pipes, sockets) — a stale registration would leave a
    /// pending wakeup for some later, unrelated block.
    pub fn wake_or_defer(&mut self, pid: usize, pending: super::WakePending) {
        let woke = self.core.wake_matching(
            |p| p.pid.0 == pid && matches!(p.state, ProcessState::Blocked),
            |p| {
                if let super::WakePending::Return(rax) = pending {
                    p.trapframe.rax = rax;
                }
                p.state = ProcessState::Ready;
            },
        );
        if woke {
            self.kick_idle(false);
        } else if let Some(p) = self.iter_running_mut().find(|p| p.pid.0 == pid) {
            p.wake_pending = Some(pending);
        }
    }

    /// For a waker that completes the waiter's operation itself (a pipe
    /// handing data to a blocked reader): runs `f` on process `pid` —
    /// Blocked, or still running on its way to blocking — and makes what it
    /// returns that process's syscall return value, then wakes it
    /// (`wake_or_defer`). `false`, with `f` never called, if `pid` is
    /// neither.
    pub fn deliver_to_waiter(&mut self, pid: usize, f: impl FnOnce(&Process) -> u64) -> bool {
        let blocked = self.core.wait_queue().iter()
            .find(|p| p.pid.0 == pid && matches!(p.state, ProcessState::Blocked));
        let target = match blocked {
            Some(p) => Some(p.as_ref()),
            None => self.iter_running().find(|p| p.pid.0 == pid),
        };
        let Some(target) = target else { return false };
        let rax = f(target);
        self.wake_or_defer(pid, super::WakePending::Return(rax));
        true
    }

    /// Called once `dead_pid` is fully dead (either already zombie-parked
    /// in `wait_queue`, or reaped immediately if it was a thread) — queues
    /// `SIGCHLD` on the parent (if there is one to notify — threads never
    /// get one, see `Process::is_thread`'s doc comment) and wakes it if
    /// it's blocked in `waitpid()` for exactly this child.
    ///
    /// Must be called with the scheduler lock already held (`&mut self`,
    /// i.e. from inside a method on `Scheduler`) and interrupts already
    /// disabled — every call site satisfies both by the time a process is
    /// fully dead. This replaces what used to be two separate operations
    /// (a manual SIGCHLD-queue block plus a freestanding `waitpid_wakeup`
    /// function that re-acquired the scheduler lock itself) so both
    /// `sys_exit` and the uncaught-signal/hardware-fault kill paths can
    /// share one correctly-locked implementation instead of each growing
    /// their own copy.
    pub fn notify_child_death(&mut self, dead_pid: usize, parent_pid: Option<Pid>) {
        if let Some(parent_pid) = parent_pid {
            if self.current_pid() == Some(parent_pid) {
                if let Some(parent) = self.running_mut() {
                    super::signal::queue_signal(parent, super::signal::SIGCHLD);
                }
            } else if let Some(parent) = self.find_process_mut(parent_pid.0) {
                super::signal::queue_signal(parent, super::signal::SIGCHLD);
            }
        }

        // Real exit status, if `dead_pid` is parked as a zombie. Threads
        // aren't (reaped immediately in `kill_current`), so this stays at
        // the "exited(0)" default for them — matches this kernel's existing
        // stance that nothing meaningful ever `waitpid()`s a thread's tid.
        let dead = self.core.wait_queue().iter()
            .find(|p| p.pid.0 == dead_pid && matches!(p.state, ProcessState::Zombie));
        let status_word = dead.map(|p| p.wait_status_word()).unwrap_or(0x200);
        let dead_pgid = dead.map(|p| p.pgid).unwrap_or(0);

        // Only the real parent can be woken — `WaitTarget::AnyChild`/`Pgid`
        // still must not wake an unrelated process just because its own
        // `waitpid()` target happens to match by pid/pgid coincidence.
        let mut waker_pid: Option<usize> = None;
        for proc in self.core.wait_queue_mut().iter_mut() {
            if Some(proc.pid) == parent_pid
                && matches!(proc.state, ProcessState::Blocked)
                && proc.waiting_for.map(|t| t.matches(dead_pid, dead_pgid)).unwrap_or(false)
            {
                proc.trapframe.rax = dead_pid as u64;
                proc.waiting_for = None;
                proc.pending_wait_status = Some(status_word);
                waker_pid = Some(proc.pid.0);
                break;
            }
        }
        if let Some(pid) = waker_pid {
            // The parent's `waitpid` returns this child's pid without running
            // again, so it is reaped here — as the zombie branch of
            // `sys_waitpid` would have. Left in the queue, it stayed a zombie
            // until some later `waitpid` happened to find it, reporting the
            // same pid twice; PID 1 never looked again, so `busybox
            // --install`'s zombie lived for the whole uptime.
            self.reap_zombie(dead_pid);
            self.wake(pid);
        }
        // Last: a parent whose `waitpid` this death just completed must get
        // its child's pid, not EINTR from the SIGCHLD queued above — the
        // call completes and the handler runs after it, as on Linux.
        self.interrupt_blocked();
    }

    /// Remove zombie `pid` from the wait queue and free it. Its kernel stack
    /// goes through `pending_stack_frees`: its `sys_exit` may still be
    /// unwinding on it on another CPU. Its address space is in no CR3 —
    /// every CPU switches away from a dying process under this lock.
    pub fn reap_zombie(&mut self, pid: usize) -> bool {
        let Some(pos) = self.core.wait_queue().iter()
            .position(|p| p.pid.0 == pid && matches!(p.state, ProcessState::Zombie))
        else {
            return false;
        };
        let proc = self.core.wait_queue_mut().remove(pos).unwrap();
        self.credit_reaped(&proc);
        self.defer_stack_free(proc.kernel_stack);
        crate::debug::inc_reaps();
        true
    }

    /// Hand every child of `dead` to PID 1, as Linux does with orphans, and
    /// tell PID 1 about the ones already dead (SIGCHLD, and a wakeup if it
    /// is blocked in a `waitpid` they match). Without this a process that
    /// forked and exited without reaping left its children's zombies — and
    /// their kernel stacks — in the wait queue forever, since `waitpid`
    /// only ever matches on `parent_pid`. Threads are skipped: they are
    /// never waited for (`kill_current` frees them at once).
    fn reparent_children(&mut self, dead: Pid) {
        if dead.0 == INIT_PID {
            return;
        }
        let mut zombies: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
        let mut adopt = |p: &mut Process| {
            if p.parent_pid == Some(dead) && !p.is_thread {
                p.parent_pid = Some(Pid(INIT_PID));
                if matches!(p.state, ProcessState::Zombie) {
                    zombies.push(p.pid.0);
                }
            }
        };
        for p in self.running.iter_mut().filter_map(|r| r.as_deref_mut()) {
            adopt(p);
        }
        for p in self.core.iter_queued_mut() {
            adopt(p);
        }
        for z in zombies {
            self.notify_child_death(z, Some(Pid(INIT_PID)));
        }
    }

    /// Called once a child transitions to `ProcessState::Stopped` (SIGSTOP/
    /// SIGTSTP) — queues `SIGCHLD` on the parent (matches real POSIX: a
    /// child stopping is also a `SIGCHLD`-worthy event, not just exiting)
    /// and wakes the parent if it's blocked in a `WUNTRACED` `waitpid()`
    /// matching this pid/pgid. Unlike `notify_child_death`, the stopped
    /// process is NOT removed from `wait_queue` — it stays there so a later
    /// real exit, or another stop/continue cycle, can still be observed.
    pub fn notify_child_stopped(&mut self, stopped_pid: usize, parent_pid: Option<Pid>) {
        if let Some(parent_pid) = parent_pid {
            if self.current_pid() == Some(parent_pid) {
                if let Some(parent) = self.running_mut() {
                    super::signal::queue_signal(parent, super::signal::SIGCHLD);
                }
            } else if let Some(parent) = self.find_process_mut(parent_pid.0) {
                super::signal::queue_signal(parent, super::signal::SIGCHLD);
            }
        }

        let Some((stopped_pgid, status_word)) = self.core.wait_queue().iter()
            .find(|p| p.pid.0 == stopped_pid && matches!(p.state, ProcessState::Stopped))
            .map(|p| (p.pgid, p.stop_status_word()))
        else {
            self.interrupt_blocked();
            return;
        };

        const WUNTRACED: i32 = 4;
        let mut waker_pid: Option<usize> = None;
        for proc in self.core.wait_queue_mut().iter_mut() {
            if Some(proc.pid) == parent_pid
                && matches!(proc.state, ProcessState::Blocked)
                && proc.waiting_options & WUNTRACED != 0
                && proc.waiting_for.map(|t| t.matches(stopped_pid, stopped_pgid)).unwrap_or(false)
            {
                proc.trapframe.rax = stopped_pid as u64;
                proc.waiting_for = None;
                proc.pending_wait_status = Some(status_word);
                waker_pid = Some(proc.pid.0);
                break;
            }
        }
        if let Some(pid) = waker_pid {
            // One-shot: don't let a future waitpid() scan re-report the
            // same stop event (see `Process::stop_reported`'s doc comment).
            if let Some(p) = self.core.wait_queue_mut().iter_mut().find(|p| p.pid.0 == stopped_pid) {
                p.stop_reported = true;
            }
            self.wake(pid);
        }
        // Last, for the reason `notify_child_death` gives.
        self.interrupt_blocked();
    }

    /// If the process about to resume (`self.running`) has a pending
    /// reaped-child wait status (stashed by `notify_child_death`, possibly
    /// while a completely different process's page table was active, since
    /// a dying child can't safely write into its blocked parent's user
    /// memory directly), write it into the user pointer that process
    /// originally passed to `waitpid()`, now that its own address space is
    /// active again.
    ///
    /// Called from every "about to return to user mode" site (mirrors
    /// `resolve_signals`, see its call sites) — cheap no-op check when
    /// there's nothing pending, which is the common case.
    pub fn resolve_wait_status(&mut self) {
        let Some(proc) = self.running_mut() else { return; };
        let Some(status) = proc.pending_wait_status.take() else { return; };
        if proc.waiting_status_ptr != 0 {
            // write_unaligned: this pointer only passed the coarse
            // in-canonical-range check in `validate_user_buffer` when
            // waitpid() first blocked — not an alignment or mapping check.
            // A misaligned-but-in-range value (e.g. from a caller that
            // forgot to zero this syscall arg — see the userspace shell's
            // waitpid() wrapper bug this was found from) must not be able
            // to panic the kernel via `write`'s UB precondition here.
            unsafe {
                core::ptr::write_unaligned(proc.waiting_status_ptr as *mut i32, status);
            }
        }
        proc.waiting_status_ptr = 0;
    }

    // ====================================================================
    // Timer tick
    // ====================================================================

    /// Called on every timer tick, on every CPU that schedules.  Returns
    /// true if a context switch should happen (time slice exhausted, or this
    /// CPU is idle and something it may take is Ready).
    ///
    /// `interrupted_rsp` is the interrupted frame's saved RSP — the deepest
    /// address the preempted code had pushed to. It guards the deferred
    /// kernel-stack frees below. `user_mode`: the tick interrupted ring 3,
    /// which is what charges it as user rather than system time.
    pub fn tick(&mut self, interrupted_rsp: u64, user_mode: bool) -> bool {
        let me = crate::cpu::cpu_id();
        // Aging is global work (decision 3 of docs/smp/smp-plan.md): one
        // core, one aging clock, advanced by CPU 0's tick only — every
        // CPU's tick bumping it would age N times as fast.
        let aging_due = if me == 0 {
            TICKS[0].fetch_add(1, Ordering::Relaxed);
            self.core.advance_ticks(&KernelClock)
        } else {
            false
        };

        // Deferred kernel-stack frees. A queued stack can still be in use:
        // by this CPU (`sys_exit`'s epilogue runs on the dying process's
        // stack — the `interrupted_rsp` check), or by another CPU that has
        // switched away from it but not yet left it (`LEAVING`; a zombie
        // reaped by `waitpid` on one CPU while its exit is still unwinding
        // on another). Either way it stays queued for a later tick.
        //
        // Still must use try_free (non-blocking): this runs inside the
        // timer ISR, which can interrupt code that already holds the
        // Buddy lock without having disabled interrupts (nothing before
        // this ever called into Buddy from an ISR). Entries that lose the
        // race just stay queued for the next tick.
        self.pending_stack_frees.retain(|&stack_top| {
            let top = stack_top.as_u64();
            let lo = top - (1u64 << crate::init::processes::KERNEL_STACK_ORDER);
            if (lo..top).contains(&interrupted_rsp) || stack_in_use(top) {
                return true;
            }
            !crate::init::processes::try_free_kernel_stack(stack_top)
        });
        // Same reasoning as pending_stack_frees above — see try_free_huge_vma's
        // doc comment for why this specific free needs the try_lock treatment.
        self.pending_vma_frees.retain(|(address_space, start, size_pages)| {
            !unsafe { address_space.try_free_huge_vma(*start, *size_pages) }
        });

        if aging_due {
            self.core.age_processes();
        }

        // The load average (global work, CPU 0): runnable = running on
        // some CPU or waiting in a run queue, sampled every 5 s.
        if me == 0 && TICKS[0].load(Ordering::Relaxed) % sched::loadavg::LOAD_FREQ == 0 {
            let runnable = self.iter_running().count() + self.core.iter_ready_desc().count();
            let mut l = sched::loadavg::LoadAvg {
                avg: core::array::from_fn(|i| LOADAVG[i].load(Ordering::Relaxed)),
            };
            l.sample(runnable as u64);
            for (slot, v) in LOADAVG.iter().zip(l.avg) {
                slot.store(v, Ordering::Relaxed);
            }
        }

        let idle = self.running[me].as_ref().map_or(true, |p| p.pid.0 == 0);
        let kind = sched::cputime::classify(user_mode, idle);
        match kind {
            sched::cputime::TickKind::User => &CPU_USER_TICKS[me],
            sched::cputime::TickKind::System => &CPU_SYSTEM_TICKS[me],
            sched::cputime::TickKind::Idle => &CPU_IDLE_TICKS[me],
        }
        .fetch_add(1, Ordering::Relaxed);
        if idle {
            return self.idle_should_switch(me);
        }
        if let Some(p) = self.running[me].as_mut() {
            p.times.charge(kind);
        }
        self.core.consume_quantum_on(me)
    }

    /// This CPU is idle: is there anything Ready it may take? Not while its
    /// idle process runs a `smp::run_on` job, which must finish there.
    fn idle_should_switch(&self, me: usize) -> bool {
        !crate::smp::ap_busy(me) && self.core.iter_ready_desc().any(|p| eligible(me, p))
    }

    /// For the reschedule IPI: switch now if this CPU is idle and has work.
    pub fn resched_due(&self) -> bool {
        let me = crate::cpu::cpu_id();
        match self.running[me].as_ref() {
            Some(p) if p.pid.0 == 0 => self.idle_should_switch(me),
            _ => false,
        }
    }

    // ====================================================================
    // Context switch
    // ====================================================================

    /// Save current process, find next Ready, activate, return new TrapFrame.
    pub fn switch_to_next(&mut self, current_tf: *const TrapFrame) -> *const TrapFrame {
        let me = crate::cpu::cpu_id();
        // ── 1. Save current process back to its run queue ─────────────
        if let Some(mut proc) = self.running[me].take() {
            tf_note_save(&mut proc, "switch_to_next");
            unsafe { *proc.trapframe = *current_tf; }
            proc.fs_base = read_fs_base();
            unsafe { super::fpu::save(&mut proc.fpu_state); }
            note_leaving(&mut proc);

            if proc.pid.0 == 0 {
                // Idle goes back to its CPU's slot, never to a queue.
                proc.state = ProcessState::Ready;
                self.idle[me] = Some(proc);
            } else {
                match proc.state {
                    ProcessState::Running => {
                        // Normal preemption — put back in run queue as Ready.
                        // Priority decay itself now lives in
                        // `SchedCore::requeue_preempted`.
                        proc.state = ProcessState::Ready;
                        self.core.requeue_preempted(proc);
                    }
                    ProcessState::Zombie | ProcessState::Blocked | ProcessState::Stopped => {
                        // Process was killed, blocked, or stopped (job control)
                        // during its slice.
                        self.core.park(proc);
                    }
                    ProcessState::Ready => {
                        self.core.requeue_ready(proc);
                    }
                }
            }
        }

        // ── 2. Highest effective-priority Ready process this CPU may take,
        // or its idle process ──────────────────────────────────────────
        let next = self.pick_next();
        let tf = self.switch_in(next, "switch_to_next");
        // Whatever this CPU just put back is Ready work for an idle one.
        self.kick_idle(true);
        tf
    }

    /// The next process this CPU runs: the highest-priority Ready process
    /// no other CPU is still leaving (`eligible`), else this CPU's idle.
    fn pick_next(&mut self) -> Box<Process> {
        let me = crate::cpu::cpu_id();
        let mut skipped = false;
        let picked = self.core.pop_next_ready_where(|p| {
            let ok = eligible(me, p);
            skipped |= !ok;
            ok
        });
        if skipped {
            LEAVING_SKIPS.fetch_add(1, Ordering::Relaxed);
        }
        match picked {
            Some(p) => p,
            None => self.idle[me].take().expect("this CPU has no idle process"),
        }
    }

    /// Make `proc` this CPU's running process: its address space, kernel
    /// stack, FS base, FPU state, a fresh slice. Returns its saved frame.
    fn switch_in(&mut self, mut proc: Box<Process>, site: &'static str) -> *const TrapFrame {
        let me = crate::cpu::cpu_id();
        proc.state = ProcessState::Running;
        proc.run_since_ns = crate::time::ktime_get();
        proc.last_cpu = me;
        unsafe { proc.address_space.activate(); }
        // The table this CPU had loaded is not its CR3 any more.
        drop(self.retiring[me].take());
        super::tss::set_kernel_stack(proc.kernel_stack);
        write_fs_base(proc.fs_base);
        unsafe { super::fpu::restore(&proc.fpu_state); }
        crate::debug::inc_switches();
        CPU_SWITCHES[me].fetch_add(1, Ordering::Relaxed);

        self.core.start_slice_on(me, proc.effective_priority);

        tf_note_resume(&mut proc, site);
        let tf_ptr = &*proc.trapframe as *const TrapFrame;
        update_current_fast(&proc);
        self.running[me] = Some(proc);
        self.note_concurrency(me);
        tf_ptr
    }

    /// Feeds `MAX_CONCURRENT`/`MAX_SAME_AS` after `me` took a process.
    fn note_concurrency(&self, me: usize) {
        let Some(p) = self.running[me].as_deref() else { return };
        if p.pid.0 == 0 {
            return;
        }
        let space = alloc::sync::Arc::as_ptr(&p.address_space);
        let (mut busy, mut same) = (0u64, 0u64);
        for q in self.iter_running() {
            busy += 1;
            if alloc::sync::Arc::as_ptr(&q.address_space) == space {
                same += 1;
            }
        }
        MAX_CONCURRENT.fetch_max(busy, Ordering::Relaxed);
        MAX_SAME_AS.fetch_max(same, Ordering::Relaxed);
    }

    /// Something is Ready: send the reschedule IPI to one idle CPU so it
    /// does not wait for its next tick. This CPU first, unless `exclude_me`
    /// — an interrupt that woke a process while this CPU idles is the case
    /// that wants it, the IPI then taken the moment the interrupt returns.
    fn kick_idle(&self, exclude_me: bool) {
        if !self.core.has_ready() || !crate::interrupts::apic::active() {
            return;
        }
        let me = crate::cpu::cpu_id();
        let idle_on = |c: usize| {
            is_scheduling(c)
                && self.running[c].as_ref().map_or(false, |p| p.pid.0 == 0)
                && !crate::smp::ap_busy(c)
        };
        let target = if !exclude_me && idle_on(me) {
            Some(me)
        } else {
            (0..MAX_CPUS).find(|&c| c != me && idle_on(c))
        };
        if let Some(c) = target {
            RESCHED_IPIS.fetch_add(1, Ordering::Relaxed);
            let apic_id = if c == me {
                crate::interrupts::apic::this_lapic_id()
            } else {
                crate::smp::apic_id(c)
            };
            crate::interrupts::apic::send_ipi(apic_id, hal::smp::icr::fixed(RESCHED_VECTOR));
        }
    }

    // ====================================================================
    // Boot: start first process
    // ====================================================================

    pub fn start_first(&mut self) -> *const TrapFrame {
        crate::serial_println!("Available processes:");
        for proc in self.core.iter_ready_desc() {
            crate::serial_println!(
                "  PID {} (base pri {}, eff {}): {:?} - {:?}",
                proc.pid.0,
                proc.priority,
                proc.effective_priority,
                core::str::from_utf8(&proc.name)
                    .unwrap_or("<?>")
                    .trim_end_matches('\0'),
                proc.privilege,
            );
        }

        if let Some(proc) = self.core.take_first_startable() {
            crate::serial_println!(
                "\n🚀 Starting first process: PID {} ({})",
                proc.pid.0,
                core::str::from_utf8(&proc.name)
                    .unwrap_or("<invalid>")
                    .trim_end_matches('\0'),
            );
            let tf = self.switch_in(proc, "start_first");
            SCHEDULING[crate::cpu::cpu_id()].store(true, Ordering::Release);
            return tf;
        }

        panic!("No process to start!");
    }

    /// An AP enters the scheduler: it starts on its idle process and takes
    /// work from its first tick or reschedule IPI on.
    pub fn start_ap(&mut self) -> *const TrapFrame {
        let me = crate::cpu::cpu_id();
        let idle = self.idle[me].take().expect("AP entering the scheduler without an idle process");
        let tf = self.switch_in(idle, "start_ap");
        SCHEDULING[me].store(true, Ordering::Release);
        tf
    }
}

/// See `Scheduler::current_cpu_times`.
#[derive(Clone, Copy, Debug)]
pub struct CpuTimesOf {
    pub own: sched::cputime::ProcTimes,
    pub own_exec_ns: u64,
    pub group: sched::cputime::ProcTimes,
    pub group_exec_ns: u64,
}

/// `/proc/stat`'s `procs_running`: processes Running or Ready (idle not
/// counted), and every process there is.
pub fn process_counts() -> (usize, usize) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let s = local_scheduler();
        let running = s.iter_all()
            .filter(|p| matches!(p.state, ProcessState::Running | ProcessState::Ready))
            .count();
        (running, s.iter_all().count())
    })
}

/// The load averages (`sched::loadavg` fixed point) and the last pid
/// allocated, for `/proc/loadavg` and `sysinfo`.
pub fn loadavg() -> ([u64; 3], usize) {
    let avg = core::array::from_fn(|i| LOADAVG[i].load(Ordering::Relaxed));
    let last = x86_64::instructions::interrupts::without_interrupts(|| local_scheduler().core.last_pid());
    (avg, last)
}

/// The CPUs that run processes, in order — what `/proc/stat` lists and
/// `sched_getaffinity` reports. With `CONSTANOS_NOSMP=1` only CPU 0; the
/// APs are online there but never run a process, so for userspace they
/// are not there.
pub fn scheduling_cpus() -> impl Iterator<Item = usize> {
    (0..MAX_CPUS).filter(|&c| is_scheduling(c))
}

/// Where `cpu`'s ticks have gone since it started scheduling.
pub fn cpu_times(cpu: usize) -> sched::cputime::CpuTimes {
    sched::cputime::CpuTimes {
        user: CPU_USER_TICKS[cpu].load(Ordering::Relaxed),
        system: CPU_SYSTEM_TICKS[cpu].load(Ordering::Relaxed),
        idle: CPU_IDLE_TICKS[cpu].load(Ordering::Relaxed),
        ..Default::default()
    }
}

/// `sched:` line of `/proc/kdebug`: what each scheduling CPU runs, its
/// switches and busy/idle ticks, and the concurrency actually reached.
pub fn render() -> alloc::string::String {
    use core::fmt::Write;
    let mut out = alloc::string::String::new();
    let running: [usize; MAX_CPUS] = x86_64::instructions::interrupts::without_interrupts(|| {
        let s = local_scheduler();
        core::array::from_fn(|c| s.running[c].as_ref().map_or(usize::MAX, |p| p.pid.0))
    });
    let invariants = x86_64::instructions::interrupts::without_interrupts(|| local_scheduler().check_invariants());
    let _ = writeln!(
        out,
        "sched: nosmp={} max_concurrent={} max_threads_parallel={} resched_ipis={} leaving_skips={} invariants={}",
        crate::smp::nosmp(),
        MAX_CONCURRENT.load(Ordering::Relaxed),
        MAX_SAME_AS.load(Ordering::Relaxed),
        RESCHED_IPIS.load(Ordering::Relaxed),
        LEAVING_SKIPS.load(Ordering::Relaxed),
        match invariants { Ok(()) => alloc::string::String::from("ok"), Err(v) => alloc::format!("{:?}", v) },
    );
    for c in (0..MAX_CPUS).filter(|&c| is_scheduling(c)) {
        let busy = CPU_USER_TICKS[c].load(Ordering::Relaxed) + CPU_SYSTEM_TICKS[c].load(Ordering::Relaxed);
        let idle = CPU_IDLE_TICKS[c].load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "  cpu{}: pid {} switches {} ticks busy {} idle {}",
            c,
            if running[c] == usize::MAX { alloc::string::String::from("-") } else { alloc::format!("{}", running[c]) },
            CPU_SWITCHES[c].load(Ordering::Relaxed),
            busy,
            idle,
        );
    }
    out
}

// ============================================================================
// Public API
// ============================================================================

pub fn current_pid() -> Option<usize> {
    local_scheduler().current_pid().map(|pid| pid.0)
}

/// `Scheduler::arm` for callers outside the scheduler lock that registered
/// under a lock of their own which must not nest it (a pipe's buffer, the
/// socket waiter list): register with `cell`, drop that lock, then arm.
/// A waker that claims `cell` in between finds the process running and
/// leaves `wake_pending`. Same calling context as `current_pid`.
pub fn arm_wait(cell: alloc::sync::Arc<super::wait::WaitCell>) {
    local_scheduler().arm(cell);
}

/// A signal is interrupting `w` if it can win the wait's cell (or the
/// wait has no cell to race for: `waitpid`). Called with the scheduler lock
/// held; the cancel is final, so the caller must go on to interrupt.
fn try_interrupt(w: &super::wait::ActiveWait) -> bool {
    match w.wait.how {
        super::wait::Interruptible::No => false,
        super::wait::Interruptible::Cell(_) => w.cell.as_ref().is_some_and(|c| c.cancel()),
        super::wait::Interruptible::WaitPid => true,
    }
}

/// The rest of an interruption once `try_interrupt` has won: tidy up,
/// and leave what the call becomes to signal delivery.
fn finish_interrupt(p: &mut Process) {
    let Some(w) = p.wait.take() else { return };
    match w.wait.how {
        super::wait::Interruptible::Cell(super::wait::Cleanup::Timer(id)) => {
            crate::time::hrtimer::cancel(id);
        }
        super::wait::Interruptible::WaitPid => {
            p.waiting_for = None;
            p.waiting_options = 0;
            p.waiting_status_ptr = 0;
        }
        _ => {}
    }
    p.interrupted = Some(super::wait::Interrupted {
        nr: w.wait.nr,
        ret_rip: w.wait.ret_rip,
        policy: w.wait.policy,
    });
}

/// `block_current`'s half of "check-then-sleep is one step": a signal the
/// process would act on was queued while it was still running (so no
/// `interrupt_blocked` could find it Blocked). Then it does not sleep; the
/// call is interrupted right here, exactly as if it had slept and been
/// woken. `false` if the wait is not interruptible or a waker already
/// claimed it (then that waker wakes it).
fn interrupt_before_blocking(p: &mut Process, wait: super::wait::Wait) -> bool {
    if p.in_sigsuspend || !super::signal::has_actionable(p) {
        return false;
    }
    let active = super::wait::ActiveWait {
        wait,
        cell: match wait.how {
            super::wait::Interruptible::Cell(_) => p.armed_wait.take(),
            _ => None,
        },
    };
    if !try_interrupt(&active) {
        // Put a claimed cell back: `block_current` parks with it.
        p.armed_wait = active.cell;
        return false;
    }
    p.wait = Some(active);
    finish_interrupt(p);
    true
}

/// Same as `current_pid()`, but self-contained (`cli`/`sti` around the
/// lock) — safe to call from anywhere, not just from inside an
/// already-`cli`'d syscall body like `current_pid()` requires (see the
/// module doc's "Interrupt safety" note: holding `SCHEDULER` with
/// interrupts enabled can deadlock against the timer ISR). Used by
/// `fs::procfs`, which isn't part of the syscall dispatch path and so
/// has no surrounding `cli` to rely on.
pub fn current_pid_safe() -> Option<usize> {
    unsafe { core::arch::asm!("cli"); }
    let pid = local_scheduler().current_pid().map(|p| p.0);
    unsafe { core::arch::asm!("sti"); }
    pid
}

/// Look up an arbitrary process's `exe_name` by pid — checked against
/// `running` plus every run queue and the wait queue (see `iter_all`).
/// Self-contained `cli`/`sti`, same reasoning as `current_pid_safe`.
/// Backs `/proc/<pid>/cmdline` (`fs::procfs`): `Process::cmdline`.
pub fn cmdline_for_pid(pid: usize) -> Option<Arc<[u8]>> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        local_scheduler().iter_all().find(|p| p.pid.0 == pid).map(|p| p.cmdline.clone())
    })
}

/// Backs `/proc/<pid>/exe`'s `readlink()` (`fs::procfs`).
pub fn exe_name_for_pid(pid: usize) -> Option<alloc::string::String> {
    unsafe { core::arch::asm!("cli"); }
    let name = local_scheduler().iter_all()
        .find(|p| p.pid.0 == pid)
        .map(|p| p.exe_name.clone());
    unsafe { core::arch::asm!("sti"); }
    name
}

/// Every live pid (running + every run queue + the wait queue, via
/// `iter_all`) — backs `/proc`'s `readdir()` (`fs::procfs`), which is what
/// lets `ls /proc` / BusyBox `ps`'s `opendir("/proc")` scan see every
/// process instead of only the ones looked up by exact name/pid.
pub fn all_pids() -> alloc::vec::Vec<usize> {
    unsafe { core::arch::asm!("cli"); }
    let pids = local_scheduler().iter_all().map(|p| p.pid.0).collect();
    unsafe { core::arch::asm!("sti"); }
    pids
}

/// Snapshot of the `Process` fields `/proc/<pid>/stat` needs to report
/// (`fs::procfs`) — the classic Linux `stat` format BusyBox `ps`/`top`
/// parse (`comm`, one-char state, ppid, pgid). Copied out under the same
/// `cli`/lock scope as `exe_name_for_pid` rather than returning a
/// reference, for the same reason: the process could be reaped the moment
/// the lock is released.
pub struct ProcStatSnapshot {
    pub ppid: usize,
    pub pgid: u32,
    pub sid: u32,
    pub name: [u8; 16],
    pub state: crate::process::ProcessState,
    pub priority: u8,
    pub times: sched::cputime::ProcTimes,
    pub start_ticks: u64,
    pub last_cpu: usize,
    pub vsize: u64,
    /// Kernel layout: bit N = signal N.
    pub pending: u64,
    pub blocked: u64,
    pub ctty: Option<usize>,
}

pub fn proc_stat_snapshot(pid: usize) -> Option<ProcStatSnapshot> {
    unsafe { core::arch::asm!("cli"); }
    let snap = local_scheduler().iter_all()
        .find(|p| p.pid.0 == pid)
        .map(|p| ProcStatSnapshot {
            ppid: p.parent_pid.map(|pp| pp.0).unwrap_or(0),
            pgid: p.pgid,
            sid: p.sid,
            name: p.name,
            state: p.state,
            priority: p.effective_priority,
            times: {
                let mut t = p.times;
                t.absorb_thread(&p.dead_threads);
                t
            },
            start_ticks: p.start_ticks,
            last_cpu: p.last_cpu,
            vsize: p.address_space.vsize_bytes(),
            pending: p.pending_signals,
            blocked: p.blocked_signals,
            ctty: p.ctty,
        });
    unsafe { core::arch::asm!("sti"); }
    snap
}

pub fn find_current_vma(addr: u64) -> Option<(usize, Vma)> {
    let scheduler = local_scheduler();
    let proc = scheduler.running_ref()?;
    let vma = proc.address_space.find_vma(addr)?;
    Some((proc.pid.0, vma))
}

/// Fast access to the running process's AddressSpace without the Mutex.
///
/// Same safety invariants as `find_vma_fast`.
///
/// # Safety
/// Must be called with interrupts disabled.
pub unsafe fn current_as_fast() -> Option<&'static AddressSpace> {
    let cpu = crate::cpu::cpu_id();
    let as_ptr = CURRENT_AS_PTR[cpu].load(Ordering::Acquire) as *const AddressSpace;
    if as_ptr.is_null() {
        None
    } else {
        Some(&*as_ptr)
    }
}

/// Fast PID read for logging (no Mutex).
pub fn current_pid_fast() -> usize {
    CURRENT_PID_FAST[crate::cpu::cpu_id()].load(Ordering::Relaxed)
}

/// The adapter half of the `sched::SchedEntity` seam (see that trait's doc
/// comment). This impl must stay a pure field-accessor — no logic, no
/// locking, nothing that could differ from what `sched`'s core assumes
/// about how these methods behave. Any behavior beyond "read/write this one
/// field" belongs in the core itself (once later extraction steps move it
/// there), not here.
impl sched::SchedEntity for Process {
    fn pid(&self) -> usize {
        self.pid.0
    }

    fn base_priority(&self) -> u8 {
        self.priority
    }

    fn effective_priority(&self) -> u8 {
        self.effective_priority
    }

    fn set_effective_priority(&mut self, pri: u8) {
        self.effective_priority = pri;
    }

    fn is_idle(&self) -> bool {
        self.pid.0 == 0
    }

    fn is_ready(&self) -> bool {
        self.state == ProcessState::Ready
    }
}
