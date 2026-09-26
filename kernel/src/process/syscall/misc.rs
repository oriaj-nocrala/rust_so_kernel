// kernel/src/process/syscall/misc.rs
//
// Small standalone syscalls that don't fit any other subsystem: uptime/
// meminfo/kdebug_ctl (custom, above the Linux syscall range) and
// clock_gettime (Linux #228).

use super::{errno, SyscallResult, validate_user_buffer};

pub(super) fn sys_uptime_ms() -> SyscallResult {
    crate::cpu::tsc::uptime_ms() as SyscallResult
}

/// sys_meminfo_kb (custom #402) — free physical memory, in KiB.
///
/// Mainly a debugging aid: run something in a loop (e.g. `sh` a script that
/// spawns/kills threads or processes many times) and watch this between
/// runs to catch a leak — see kernel_stack's `pending_stack_frees` /
/// `try_free_kernel_stack` for the leak this was added to verify.
pub(super) fn sys_meminfo_kb() -> SyscallResult {
    (crate::allocator::free_bytes() / 1024) as SyscallResult
}

/// sys_kdebug_ctl (custom #403): long kdebug_ctl(int cmd, const char *name, int enable)
///
/// Runtime control for `crate::debug`'s tracing subsystems (see that
/// module's doc comment for why this exists — replaces hand-added-then-
/// stripped-out `serial_println!` debugging with tracepoints that stay in
/// the code permanently, toggled live instead of by rebuilding). Backs the
/// `kdebug` userspace program.
///
/// `cmd`: 0 = get current mask (other args ignored). 1 = set: resolve
/// `name` (a NUL-terminated string, e.g. "mm") to its subsystem bit and
/// set or clear it in the mask depending on `enable`; returns the *new*
/// mask, or `EINVAL` if `name` doesn't match a known subsystem. 2 = panic
/// the kernel on purpose (never returns).
pub(super) fn sys_kdebug_ctl(cmd: u64, name_ptr: u64, enable: u64) -> SyscallResult {
    match cmd {
        0 => crate::debug::get_mask() as SyscallResult,
        1 => {
            if let Err(e) = validate_user_buffer(name_ptr, 32) {
                return e;
            }
            let name_bytes = unsafe {
                let ptr = name_ptr as *const u8;
                let mut len = 0usize;
                while len < 32 {
                    if *ptr.add(len) == 0 { break; }
                    len += 1;
                }
                core::slice::from_raw_parts(ptr, len)
            };
            let name = match core::str::from_utf8(name_bytes) {
                Ok(s) => s,
                Err(_) => return errno::EINVAL,
            };
            let Some(bit) = crate::debug::subsystem_bit_by_name(name) else {
                return errno::EINVAL;
            };
            let mut mask = crate::debug::get_mask();
            if enable != 0 { mask |= bit; } else { mask &= !bit; }
            crate::debug::set_mask(mask);
            mask as SyscallResult
        }
        // A deliberate kernel panic — Linux's `echo c > /proc/sysrq-trigger`.
        // Exists to exercise the panic path end to end on demand, above all
        // the panic handler's flush of the log to the USB stick
        // (`block::logpart::on_panic`), which is otherwise only reached by
        // a real bug. `kdebug panic`.
        2 => panic!("kdebug panic: deliberate panic requested from userspace"),
        // The TLB-shootdown self-test (`tlb_selftest`) against every online
        // AP: 0 = no stale read, 1 = stale reads, ENODEV = no AP, ETIMEDOUT
        // = an AP stopped answering. The report goes to the kernel log.
        // `kdebug tlbtest`. With IF=0 throughout: since stage 7 this
        // syscall can run on an AP and be preempted onto another CPU
        // mid-test, and the writer must stay on the CPU it excluded from
        // the readers (the self-test's waits answer shootdowns themselves).
        3 => match x86_64::instructions::interrupts::without_interrupts(|| {
            crate::tlb_selftest::run(200, crate::cpu::MAX_CPUS)
        }) {
            Ok(r) if r.stale == 0 => 0,
            Ok(_) => 1,
            Err("no online AP") => errno::ENODEV,
            Err(e) => {
                crate::serial_println!("tlb_selftest: ERROR {}", e);
                errno::ETIMEDOUT
            }
        },
        _ => errno::EINVAL,
    }
}


/// sys_sync (Linux #162) — this kernel has no write-back cache to flush
/// (ext2 writes are synchronous), so what `sync` does here is the one
/// thing that *is* buffered: copy the kernel log ring to the USB stick's
/// `constanos-log` partition (`block::logpart`), the "write it now" before
/// rebooting the machine to read it.
///
/// Unlike Linux's `sync`, which cannot fail, this reports what happened —
/// `kdebug sync` prints it: `ENODEV` when there is no log partition in
/// use, `EBUSY` if a flush is already running, `EIO` if the write failed.
pub(super) fn sys_sync() -> SyscallResult {
    use crate::block::logpart::{flush, FlushError};
    match flush(hal::logpart::Reason::Sync) {
        Ok(_) => 0,
        Err(FlushError::NoPartition) => errno::ENODEV,
        Err(FlushError::Busy) => errno::EBUSY,
        Err(FlushError::Io(e)) => {
            crate::serial_println!("klog-disk: sync failed: {}", e);
            errno::EIO
        }
    }
}

/// sys_reboot (Linux #169) — `reboot(magic, magic2, cmd, arg)` with
/// Linux's magic numbers and command values, so mlibc/BusyBox callers work
/// unchanged. See `crate::reboot` for what "safe" means here.
///
/// `RESTART` resets the machine; `HALT` and `POWER_OFF` both stop it after
/// the same log flush (no ACPI S5 without an AML interpreter — Linux does
/// the same when it has no power-off driver). `CAD_ON`/`CAD_OFF` are
/// accepted and ignored: there is no Ctrl-Alt-Del handling to toggle.
/// No permission check — every process is root here.
pub(super) fn sys_reboot(magic1: u32, magic2: u32, cmd: u32) -> SyscallResult {
    const MAGIC1: u32 = 0xfee1_dead;
    const MAGIC2: [u32; 4] = [672_274_793, 85_072_278, 369_367_448, 537_993_216];
    const CMD_RESTART: u32 = 0x0123_4567;
    const CMD_HALT: u32 = 0xCDEF_0123;
    const CMD_POWER_OFF: u32 = 0x4321_FEDC;
    const CMD_CAD_ON: u32 = 0x89AB_CDEF;
    const CMD_CAD_OFF: u32 = 0;

    if magic1 != MAGIC1 || !MAGIC2.contains(&magic2) {
        return errno::EINVAL;
    }
    match cmd {
        CMD_RESTART => crate::reboot::restart(),
        CMD_HALT | CMD_POWER_OFF => crate::reboot::halt(),
        CMD_CAD_ON | CMD_CAD_OFF => 0,
        _ => errno::EINVAL,
    }
}

/// sys_uptime_sec (custom #202) — seconds elapsed since kernel boot.
///
/// Uses the active clocksource (TSC when available).
pub(super) fn sys_uptime_sec() -> SyscallResult {
    (crate::time::ktime_get() / 1_000_000_000) as SyscallResult
}

// Linux clock ids (`<time.h>`).
const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;
const CLOCK_PROCESS_CPUTIME_ID: u64 = 2;
const CLOCK_THREAD_CPUTIME_ID: u64 = 3;
const CLOCK_MONOTONIC_RAW: u64 = 4;
const CLOCK_REALTIME_COARSE: u64 = 5;
const CLOCK_MONOTONIC_COARSE: u64 = 6;
const CLOCK_BOOTTIME: u64 = 7;

/// sys_clock_gettime (Linux #228). `CLOCK_REALTIME` is wall-clock (the
/// boot-time RTC reading plus uptime); the monotonic family and
/// `CLOCK_BOOTTIME` are uptime (nothing here suspends, so they agree);
/// `CLOCK_PROCESS_CPUTIME_ID` is the time the calling process's whole
/// thread group has run and `CLOCK_THREAD_CPUTIME_ID` the caller's own,
/// measured at every context switch (`Process::exec_ns`), not sampled.
/// Other ids (a `clockid_t` for another process's CPU clock, the alarm
/// clocks) are `EINVAL`.
pub(super) fn sys_clock_gettime(clk_id: u64, tp_ptr: u64) -> SyscallResult {
    if let Err(e) = validate_user_buffer(tp_ptr, 16) {
        return e;
    }

    let (tv_sec, tv_nsec) = match clk_id {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE => {
            // `tv_nsec` is uptime's sub-second fraction: the RTC reading
            // only ever contributes whole seconds, so it is also real
            // time's fraction of its current second.
            let up = crate::time::ktime_get();
            (crate::time::now_unix_secs(), up % 1_000_000_000)
        }
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME => {
            let up = crate::time::ktime_get();
            (up / 1_000_000_000, up % 1_000_000_000)
        }
        CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID => {
            let Some(t) = super::current_cpu_times() else { return errno::ESRCH };
            let ns = if clk_id == CLOCK_PROCESS_CPUTIME_ID { t.group_exec_ns } else { t.own_exec_ns };
            (ns / 1_000_000_000, ns % 1_000_000_000)
        }
        _ => return errno::EINVAL,
    };

    // Direct write into user VA — safe because:
    //   1. validate_user_buffer confirmed it is in user-space range.
    //   2. The running process's CR3 is still active (we're in the kernel
    //      but the user page tables haven't been switched away).
    //   3. If the page isn't mapped yet, the write faults and the page-fault
    //      handler demand-pages it (same as any user store instruction).
    // No lock is held here (`current_cpu_times` released the scheduler's).
    unsafe {
        let ptr = tp_ptr as *mut i64;
        ptr.write(tv_sec as i64);
        ptr.add(1).write(tv_nsec as i64);
    }

    0
}

/// sys_clock_getres (Linux #229): 1 ns for every clock `clock_gettime`
/// serves — each is a TSC reading converted to nanoseconds (or the jiffies
/// fallback, which is coarser than it claims; Linux reports the hrtimer
/// resolution the same way). A null `res` only validates the id.
pub(super) fn sys_clock_getres(clk_id: u64, res_ptr: u64) -> SyscallResult {
    if clk_id > CLOCK_BOOTTIME {
        return errno::EINVAL;
    }
    if res_ptr != 0 {
        if let Err(e) = validate_user_buffer(res_ptr, 16) {
            return e;
        }
        unsafe {
            let ptr = res_ptr as *mut i64;
            ptr.write(0);
            ptr.add(1).write(1);
        }
    }
    0
}

/// sys_times (Linux #100): `struct tms` (four `clock_t`s, in ticks of
/// `sched::cputime::USER_HZ`) for the calling process's thread group —
/// user, system, and its waited-for children's — and the uptime in the
/// same ticks as the return value (POSIX: "elapsed real time … since an
/// arbitrary point in the past"; Linux's is boot-relative too). A null
/// `buf` just returns the clock, as Linux allows.
pub(super) fn sys_times(buf: u64) -> SyscallResult {
    let now = (crate::time::ktime_get() / (1_000_000_000 / sched::cputime::USER_HZ)) as i64;
    if buf == 0 {
        return now;
    }
    if let Err(e) = validate_user_buffer(buf, 32) {
        return e;
    }
    let Some(t) = super::current_cpu_times() else { return errno::ESRCH };
    let g = t.group;
    unsafe {
        let p = buf as *mut i64;
        p.write(g.utime as i64);
        p.add(1).write(g.stime as i64);
        p.add(2).write(g.cutime as i64);
        p.add(3).write(g.cstime as i64);
    }
    now
}

/// `sizeof(struct sysinfo)` on x86-64.
const SYSINFO_SIZE: usize = 112;

/// sys_sysinfo (Linux #99): uptime in seconds, the load averages (scaled
/// to `1 << 16`, Linux's `SI_LOAD_SHIFT`), total and free RAM in bytes
/// (`mem_unit` 1; the Buddy allocator's view, as `/proc/meminfo`), and the
/// number of processes. No swap, no shared or buffer memory, no highmem.
pub(super) fn sys_sysinfo(buf: u64) -> SyscallResult {
    if let Err(e) = validate_user_buffer(buf, SYSINFO_SIZE) {
        return e;
    }
    let (avg, _) = crate::process::scheduler::loadavg();
    let (_, procs) = crate::process::scheduler::process_counts();
    let (total, free) = crate::allocator::mem_stats();
    let uptime = crate::time::ktime_get() / 1_000_000_000;
    let shift = 16 - sched::loadavg::FSHIFT;
    unsafe {
        let p = buf as *mut u8;
        core::ptr::write_bytes(p, 0, SYSINFO_SIZE);
        let q = p as *mut u64;
        q.write(uptime);
        for (i, a) in avg.iter().enumerate() {
            q.add(1 + i).write(a << shift);
        }
        q.add(4).write(total as u64);
        q.add(5).write(free as u64);
        (p.add(80) as *mut u16).write(procs.min(u16::MAX as usize) as u16);
        (p.add(104) as *mut u32).write(1);
    }
    0
}

const RUSAGE_SELF: i64 = 0;
const RUSAGE_CHILDREN: i64 = -1;
const RUSAGE_THREAD: i64 = 1;
/// `sizeof(struct rusage)` on x86-64: two `timeval`s and fourteen `long`s.
const RUSAGE_SIZE: usize = 144;

/// sys_getrusage (Linux #98). `ru_utime`/`ru_stime` for `RUSAGE_SELF` (the
/// thread group), `RUSAGE_CHILDREN` (waited-for descendants) or
/// `RUSAGE_THREAD` (the caller alone), from the same tick counts as
/// `times()`, so a tick's granularity (10 ms). Every other field is 0:
/// page faults, context switches and the rest are not counted per process.
pub(super) fn sys_getrusage(who: i64, buf: u64) -> SyscallResult {
    if !matches!(who, RUSAGE_SELF | RUSAGE_CHILDREN | RUSAGE_THREAD) {
        return errno::EINVAL;
    }
    if let Err(e) = validate_user_buffer(buf, RUSAGE_SIZE) {
        return e;
    }
    let Some(t) = super::current_cpu_times() else { return errno::ESRCH };
    let (user, sys) = match who {
        RUSAGE_SELF => (t.group.utime, t.group.stime),
        RUSAGE_CHILDREN => (t.group.cutime, t.group.cstime),
        _ => (t.own.utime, t.own.stime),
    };
    let usec_per_tick = 1_000_000 / sched::cputime::USER_HZ;
    let (user, sys) = (user * usec_per_tick, sys * usec_per_tick);
    unsafe {
        let p = buf as *mut i64;
        for i in 0..RUSAGE_SIZE / 8 {
            p.add(i).write(0);
        }
        p.write((user / 1_000_000) as i64);
        p.add(1).write((user % 1_000_000) as i64);
        p.add(2).write((sys / 1_000_000) as i64);
        p.add(3).write((sys % 1_000_000) as i64);
    }
    0
}

/// sys_sched_getaffinity (Linux #204): the CPUs the process may run on —
/// every CPU that runs processes, for every process, since there is no
/// `sched_setaffinity`. Linux's rules for the buffer: `len` must hold a
/// bit per possible CPU and be a multiple of `sizeof(long)` (else
/// `EINVAL`); the return value is the bytes written — the kernel's
/// cpumask size, here 8 (`MAX_CPUS` = 32 rounded up to a `long`) — and the
/// caller's libc clears the rest. `pid` 0 is the caller; any other must
/// exist (`ESRCH`).
pub(super) fn sys_sched_getaffinity(pid: i64, len: usize, mask_ptr: u64) -> SyscallResult {
    const MASK_BYTES: usize = (crate::cpu::MAX_CPUS + 63) / 64 * 8;
    if len * 8 < crate::cpu::MAX_CPUS || len % 8 != 0 {
        return errno::EINVAL;
    }
    if pid < 0 {
        return errno::ESRCH;
    }
    if pid > 0 {
        let exists = super::with_scheduler(|s| s.iter_all().any(|p| p.pid.0 == pid as usize) as SyscallResult);
        if exists == 0 {
            return errno::ESRCH;
        }
    }
    if let Err(e) = validate_user_buffer(mask_ptr, MASK_BYTES) {
        return e;
    }
    let mut mask = [0u64; MASK_BYTES / 8];
    for c in crate::process::scheduler::scheduling_cpus() {
        mask[c / 64] |= 1 << (c % 64);
    }
    unsafe {
        let p = mask_ptr as *mut u64;
        for (i, w) in mask.iter().enumerate() {
            p.add(i).write(*w);
        }
    }
    MASK_BYTES as SyscallResult
}

