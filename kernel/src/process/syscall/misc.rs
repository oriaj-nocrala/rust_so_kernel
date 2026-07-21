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
/// `free_kernel_stack` for the leak this was added to verify.
pub(super) fn sys_meminfo_kb() -> SyscallResult {
    (crate::allocator::buddy_allocator::BUDDY.lock().free_bytes() / 1024) as SyscallResult
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
/// mask, or `EINVAL` if `name` doesn't match a known subsystem.
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
        _ => errno::EINVAL,
    }
}


/// sys_uptime_sec (custom #202) — seconds elapsed since kernel boot.
///
/// Uses the active clocksource (TSC when available).
pub(super) fn sys_uptime_sec() -> SyscallResult {
    (crate::time::ktime_get() / 1_000_000_000) as SyscallResult
}

/// sys_clock_gettime (Linux #228) — write a `struct timespec` to user memory.
///
/// Supported clock IDs:
///   0 = CLOCK_REALTIME   — real wall-clock time (CMOS RTC read once at
///                          boot, see `crate::rtc`, plus uptime since)
///   1 = CLOCK_MONOTONIC  — uptime since boot, unaffected by wall-clock
///   7 = CLOCK_BOOTTIME   — same as MONOTONIC; included for glibc compat
///
/// `struct timespec { i64 tv_sec; i64 tv_nsec; }` (16 bytes, 8-byte aligned).
///
/// The process's own page table is active during the syscall, so we can
/// write directly to the user virtual address without physical translation.
pub(super) fn sys_clock_gettime(clk_id: u64, tp_ptr: u64) -> SyscallResult {
    // Validate the user pointer (16 bytes = 2 × i64)
    if let Err(e) = validate_user_buffer(tp_ptr, 16) {
        return e;
    }

    // Accept only the clock IDs we can serve meaningfully.
    match clk_id {
        0 | 1 | 7 => {}
        _ => return errno::EINVAL,
    }

    let uptime_ns = crate::time::ktime_get();
    // `tv_nsec` is uptime's own sub-second fraction either way — for
    // CLOCK_REALTIME that's also real time's fraction of its current
    // second, since the RTC reading only ever contributes whole seconds.
    let tv_nsec = (uptime_ns % 1_000_000_000) as i64;
    let tv_sec = if clk_id == 0 {
        crate::time::now_unix_secs() as i64
    } else {
        (uptime_ns / 1_000_000_000) as i64
    };

    // Direct write into user VA — safe because:
    //   1. validate_user_buffer confirmed it is in user-space range.
    //   2. The running process's CR3 is still active (we're in the kernel
    //      but the user page tables haven't been switched away).
    //   3. If the page isn't mapped yet, the write faults and the page-fault
    //      handler demand-pages it (same as any user store instruction).
    unsafe {
        let ptr = tp_ptr as *mut i64;
        ptr.write(tv_sec);
        ptr.add(1).write(tv_nsec);
    }

    0
}

