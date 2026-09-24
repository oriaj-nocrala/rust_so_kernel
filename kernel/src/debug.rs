// kernel/src/debug.rs
//
// Runtime-toggleable tracing + a handful of permanent lifecycle counters.
//
// WHY THIS EXISTS
// ────────────────
// Before this module, debugging a subsystem meant hand-adding
// `serial_println!`/`serial_println_raw!` calls, rebuilding, reproducing,
// reading the log, then manually stripping the prints back out — thrown
// away each time, so the next bug in a *different* subsystem starts from
// zero visibility again. It also meant several subsystems (COW faults,
// address-space teardown, exec) had PERMANENT, unconditional debug prints
// left in from past sessions — always on, drowning out whatever a future
// session actually needed to see (this is exactly what made the 2026-07-19
// leak/panic investigation slow: the one relevant line was buried under
// thousands of `[COW]` lines that fire on every single page fault).
//
// This module fixes both: named subsystems, gated by a runtime bitmask
// (default: everything off — silent unless asked for), so instrumentation
// can stay in the code permanently instead of being added and removed each
// time. Toggle a subsystem live via the `kdebug_ctl` syscall (403) — no
// rebuild needed — e.g. the `kdebug` userspace program: `kdebug mm on`.
//
// ADDING A NEW TRACEPOINT
// ───────────────────────
//   crate::ktrace!(crate::debug::MM, "fork: shared {} pages", n);
//
// ADDING A NEW COUNTER
// ────────────────────
//   Add an `AtomicU64` below, a matching `inc_*`/getter, and a line in
//   `render_report()` — then it shows up in `/proc/kdebug` for free.
//
// ADDING LOCK DIAGNOSTICS FOR A NEW LOCK
// ───────────────────────────────────────
//   See `LockDiag` below — grew out of a real, hours-long hunt for a
//   single-core deadlock (SCHEDULER held, interrupts re-enabled one
//   statement too early, timer ISR spins on `local_scheduler()` forever)
//   that could only be pinned down by manually reading raw memory through
//   the QEMU monitor (cross-referencing `nm` symbol addresses, decoding
//   an ASCII file path byte-by-byte by hand). `ktrace!` couldn't have
//   caught this even turned on ahead of time: it's print-based, and a
//   print inside the acquire/release path risks perturbing the exact
//   timing the race depends on. `LockDiag` is the generalized, permanent
//   version of the ad hoc atomics that actually found it — add one
//   `static FOO_LOCK: LockDiag = LockDiag::new();` per lock worth
//   watching, call `.record_acquire(core::panic::Location::caller())` /
//   `.record_release()` around it (see `scheduler::local_scheduler()`'s
//   `TrackedSchedulerGuard` for the pattern), and add a `.render(...)`
//   line to `render_report()`. Next time: `cat /proc/kdebug` shows
//   `outstanding` (acquires − releases; anything but 0/1 means a guard
//   leaked) and exactly which call site is holding it, live, with no
//   monitor session required.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

// ── What moved to the `diag` crate, and what stayed ─────────────────────────
//
// The four always-on diagnostic *types* (`LockDiag`, `DirLockDiag`,
// `TfRewindDiag`, `IfViolationDiag`) now live in the standalone, host-
// testable `diag` crate (`diag/src/`, `cd diag && cargo test`), re-exported
// below so every call site in this kernel keeps naming them through
// `crate::debug::` exactly as before. Same split, and same reason, as
// `ext2`/`mm`/`vfs`: this crate can't run `cargo test` on the host (see
// CLAUDE.md's "QEMU integration tests"), and those types carried real
// `unsafe` logic — reconstructing a `&'static str` from a stored pointer +
// length, behind range guards (`len < 64`, `len < 512`) and `"<none yet>"`/
// `"<non-utf8>"` fallbacks — that had never been exercised by a single test.
//
// What deliberately stayed here, in the adapter: every `static` instance
// (this module owns the global state, `diag` owns none — the same shape
// `EXT2: Once<Ext2Fs>` and `BUDDY`/`SLAB_ALLOCATOR` took after their own
// extractions), the `Subsystem`/`TRACE_MASK`/`ktrace!` tracing machinery,
// all the permanent lifecycle counters, `render_report`,
// `print_panic_snapshot`, and `subsystem_bit_by_name`.
//
// One seam was needed, following `mm`'s "events come back as data, the
// adapter prints" precedent: `TfRewindDiag::record` used to
// `serial_println_raw!` inline, which `diag` can't name. It now returns a
// `RewindEvent` and `tf_record()` below reproduces that exact line. The two
// allocation-free `print_panic_line` methods are gone the same way —
// `print_panic_snapshot` formats and prints from plain accessors instead.
pub use diag::{DirLockDiag, IfViolationDiag, LockDiag, OpStat, RewindEvent, TfRewindDiag};

/// Diagnostics for the scheduler's per-CPU lock — see `scheduler::
/// local_scheduler()`, which is the only thing that acquires it.
pub static SCHEDULER_LOCK: LockDiag = LockDiag::new();

/// See `DirLockDiag`'s doc comment. `vfs::ramfs::RamDirNode::lock_entries`
/// (via the `KernelDirLockObserver` seam in `kernel/src/fs/ramfs.rs`) is
/// the only thing that acquires it.
pub static RAMFS_ENTRIES_LOCK: DirLockDiag = DirLockDiag::new();

pub static TF_REWIND: TfRewindDiag = TfRewindDiag::new();

/// Record a detected trapframe rewind and print it — the kernel-side half
/// of `diag::TfRewindDiag::record`, which can't call
/// `serial_println_raw!` itself (see the "what moved" note at the top of
/// this module, and `mm`'s identical event-as-data precedent).
///
/// Loud and immediate, not just recorded for later — a rewind is rare
/// enough (should be zero, ever) that the cost of printing on every
/// occurrence is irrelevant, unlike the per-acquire prints this
/// investigation already learned to avoid (see `RAMFS_ENTRIES_LOCK`'s doc
/// comment). Call this, not `TF_REWIND.record(...)` directly, or the
/// rewind is silently swallowed.
pub fn tf_record(pid: u64, site: &'static str, old_seq: u64, new_seq: u64) {
    let ev = TF_REWIND.record(pid, site, old_seq, new_seq);
    crate::serial_println_raw!(
        "[HANGHUNT-DIAG] TF-REWIND: pid={} site={} seq {} -> {} (stale/rewound trapframe)",
        ev.pid, ev.site, ev.old_seq, ev.new_seq,
    );
}

/// See `IfViolationDiag`'s doc comment — tracks `memory::cow.rs` accessor
/// calls reached with interrupts enabled, split one counter per accessor
/// (`inc_ref`/`dec_ref` are the real non-atomic read-modify-write
/// lost-update hazard; `get_ref` is a plain read, tracked for
/// completeness; `set_ref` is a plain write into an exclusively-owned
/// index, not itself racy — see `COW_IF_ENABLED_SET_REF` below, which
/// is *not* one of these three real violations) so a violation in one
/// doesn't hide a same-boot violation in another behind a single shared
/// "last caller".
pub static COW_IF_VIOLATIONS_INC_REF: IfViolationDiag = IfViolationDiag::new();
pub static COW_IF_VIOLATIONS_DEC_REF: IfViolationDiag = IfViolationDiag::new();
pub static COW_IF_VIOLATIONS_GET_REF: IfViolationDiag = IfViolationDiag::new();
/// `memory::cow::set_ref` reached with interrupts enabled. Despite reusing
/// `IfViolationDiag` (same counter/last-line shape as the three above, and
/// worth keeping in the same family for a symmetrical `/proc/kdebug`
/// report), this one is NOT a violation: `set_ref` never actually required
/// interrupts disabled (see its doc comment in `memory/cow.rs`), so this
/// counter is purely informational — it fires ~675 times per boot from
/// `sys_exec`/`memory/elf_loader.rs`'s normal ELF-load path, and that is
/// expected, correct behavior, not evidence of a bug to chase.
pub static COW_IF_ENABLED_SET_REF: IfViolationDiag = IfViolationDiag::new();

// ── Subsystems ───────────────────────────────────────────────────────────────

/// A named, independently-toggleable tracing subsystem.
pub struct Subsystem {
    pub bit:  u32,
    pub name: &'static str,
}

pub const MM:    Subsystem = Subsystem { bit: 1 << 0, name: "mm" };
pub const SCHED: Subsystem = Subsystem { bit: 1 << 1, name: "sched" };
pub const FS:    Subsystem = Subsystem { bit: 1 << 2, name: "fs" };
pub const PROC:  Subsystem = Subsystem { bit: 1 << 3, name: "proc" };
/// USB/xHCI bring-up detail: register readbacks, DMA addresses, every event
/// the driver did not act on. All of it was added to find one real bug (a
/// torn event-TRB read, see `kernel/src/usb/xhci.rs::next_event`) and none
/// of it is wanted on a working boot — but deleting it would mean building
/// it again from scratch for the next xHCI problem, which is exactly the
/// pattern this module exists to end. Off by default; `kdebug usb on`.
pub const USB:   Subsystem = Subsystem { bit: 1 << 4, name: "usb" };

/// All subsystems, for `kdebug list` / mask validation.
pub const ALL_SUBSYSTEMS: &[&Subsystem] = &[&MM, &SCHED, &FS, &PROC, &USB];

/// Bitmask of currently-enabled subsystems. Off by default: tracing is
/// opt-in, never spamming the log unless explicitly turned on.
static TRACE_MASK: AtomicU32 = AtomicU32::new(0);

pub fn set_mask(mask: u32) -> u32 {
    TRACE_MASK.swap(mask, Ordering::Relaxed)
}

pub fn get_mask() -> u32 {
    TRACE_MASK.load(Ordering::Relaxed)
}

#[inline]
pub fn is_enabled(bit: u32) -> bool {
    TRACE_MASK.load(Ordering::Relaxed) & bit != 0
}

/// Trace a formatted line, gated on `$sub`'s bit in `TRACE_MASK` — a no-op
/// (one relaxed atomic load + branch) when that subsystem is disabled.
/// Uses the lock-free raw serial writer (like the debug prints it
/// replaces) since tracepoints can fire from contexts (page fault handler,
/// `Drop` impls mid-teardown) where taking the buffered serial lock would
/// risk deadlock.
#[macro_export]
macro_rules! ktrace {
    ($sub:expr, $($arg:tt)*) => {
        if $crate::debug::is_enabled($sub.bit) {
            $crate::serial_println_raw!("[{}] {}", $sub.name, format_args!($($arg)*));
        }
    };
}

// ── Permanent lifecycle counters ─────────────────────────────────────────────
//
// Unlike tracing, these are always on (a handful of atomic increments is
// unconditionally cheap) and never reset — cumulative since boot, read any
// time via `/proc/kdebug` instead of re-deriving them by grepping a log.

static FORKS_TOTAL:            AtomicU64 = AtomicU64::new(0);
static EXECS_TOTAL:            AtomicU64 = AtomicU64::new(0);
static REAPS_TOTAL:            AtomicU64 = AtomicU64::new(0);
static COW_FAULTS_RESOLVED:    AtomicU64 = AtomicU64::new(0);
static COW_FAULTS_FAILED:      AtomicU64 = AtomicU64::new(0);
/// Blocks/inodes `fs::ext2::Ext2Fs::reclaim_orphans` freed at mount time —
/// bitmap-set but unreachable from the root directory, i.e. left behind
/// by an unclean shutdown mid `create`/`mkdir`/`write` (see that
/// function's doc comment). Should read `0` on any boot that followed a
/// clean shutdown; a nonzero value here is direct evidence a previous
/// session ended uncleanly, independent of whether FS tracing happened to
/// be on on this boot to catch the `ktrace!` line reporting the same
/// thing.
static ORPHAN_BLOCKS_RECLAIMED: AtomicU64 = AtomicU64::new(0);
static ORPHAN_INODES_RECLAIMED: AtomicU64 = AtomicU64::new(0);
/// Full context switches (`Scheduler::switch_to_next` landing on a
/// different process) since boot — added while verifying `process::fpu`'s
/// FPU/SSE save/restore actually exercises the switch path during
/// `fpu_test` (a per-switch `serial_println!` was tried first to check
/// this and made exec()/page-fault-heavy boot phases crawl, since it ran
/// on literally every timer preemption; a plain atomic counter is free by
/// comparison and, per this kernel's own convention of keeping bug-hunting
/// instrumentation around instead of deleting it, useful for the next
/// scheduler investigation too).
static SWITCHES_TOTAL: AtomicU64 = AtomicU64::new(0);

/// USB HID boot-keyboard reports decoded since boot (`usb::poll`). The
/// counter that separates the two failure modes of a USB keyboard that
/// types nothing: zero here means the controller is not delivering
/// transfers at all (enumeration, endpoint configuration or the event
/// ring), nonzero means reports arrive and the fault is in the decode or
/// in the pipeline below it. That distinction is otherwise unobservable on
/// a machine with no serial capture — `cat /proc/kdebug` is how it gets
/// read there.
static USB_KEY_REPORTS: AtomicU64 = AtomicU64::new(0);

/// USB keyboard scancodes decoded off the event ring but dropped because
/// the driver's pending buffer was full (`usb::xhci::PENDING_KEYS`). Should
/// stay zero: the ring is drained by whoever holds the controller lock —
/// the timer's `usb::poll`, or a USB disk transfer spinning for its own
/// completion — and only `poll` hands keys on. Nonzero means the lock was
/// held for a long stretch of typing, e.g. during a slow disk read.
static USB_KEYS_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Spurious 8259 interrupts (IRQ7/IRQ15 with no ISR bit set) since boot,
/// and real interrupts on lines this kernel never unmasks. Neither used to
/// have an IDT entry at all, so the first one on real hardware was a kernel
/// panic (GPF, error 0x13B = vector 39) — the Ryzen, 2026-09-23, starting
/// `doom`. Spurious ones are harmless and expected on metal; a nonzero
/// `unexpected_irqs` means some line got unmasked that nothing services.
static SPURIOUS_IRQS: AtomicU64 = AtomicU64::new(0);
static UNEXPECTED_IRQS: AtomicU64 = AtomicU64::new(0);
static LAST_UNEXPECTED_IRQ: AtomicU64 = AtomicU64::new(u64::MAX);

pub fn inc_spurious_irqs() { SPURIOUS_IRQS.fetch_add(1, Ordering::Relaxed); }
pub fn note_unexpected_irq(line: u8) {
    UNEXPECTED_IRQS.fetch_add(1, Ordering::Relaxed);
    LAST_UNEXPECTED_IRQ.store(line as u64, Ordering::Relaxed);
}

pub fn inc_forks()         { FORKS_TOTAL.fetch_add(1, Ordering::Relaxed); }
pub fn inc_execs()         { EXECS_TOTAL.fetch_add(1, Ordering::Relaxed); }
pub fn inc_reaps()         { REAPS_TOTAL.fetch_add(1, Ordering::Relaxed); }
pub fn inc_cow_resolved()  { COW_FAULTS_RESOLVED.fetch_add(1, Ordering::Relaxed); }
pub fn inc_cow_failed()    { COW_FAULTS_FAILED.fetch_add(1, Ordering::Relaxed); }
pub fn inc_switches()      { SWITCHES_TOTAL.fetch_add(1, Ordering::Relaxed); }
pub fn inc_usb_key_reports() { USB_KEY_REPORTS.fetch_add(1, Ordering::Relaxed); }
pub fn add_usb_keys_dropped(n: u64) { USB_KEYS_DROPPED.fetch_add(n, Ordering::Relaxed); }
pub fn add_orphans_reclaimed(blocks: u64, inodes: u64) {
    ORPHAN_BLOCKS_RECLAIMED.fetch_add(blocks, Ordering::Relaxed);
    ORPHAN_INODES_RECLAIMED.fetch_add(inodes, Ordering::Relaxed);
}

// ── Framebuffer console cost counters ────────────────────────────────────────
//
// Always on, same argument as `switches_total`: three relaxed `fetch_add`s
// and one `rdtsc` per operation is nothing beside the work being measured
// (a single `draw_char` writes 64 pixels; on the machine these exist for,
// that is 64 separate bus transactions).
//
// They exist because the console is imperceptibly fast in QEMU, where the
// framebuffer is host RAM, and takes about a second to clear the screen on
// the physical AM4 machine, where it lives across PCIe — and that machine
// has no serial capture, so the only way "it feels slow" becomes a number
// is for the kernel to count its own cost and render it somewhere
// `cat`-able. See `/proc/fbinfo`, and `diag::OpStat` for the shape of each
// line.
//
// Rendered into `/proc/fbinfo` rather than `/proc/kdebug` so that one file
// carries the whole picture — geometry, memory type and cost together,
// since on that machine the report is read by photographing the screen and
// a second file means a second photograph.

/// `Framebuffer::fill_rect` — scanline rectangle fills. `ESC[J`'s
/// clear-to-end-of-screen, `clear()`, and every glyph cell's background
/// all land here.
pub static FB_FILL_RECT: OpStat = OpStat::new();
/// `Framebuffer::draw_char` — one glyph, composed a pixel row at a time.
pub static FB_DRAW_CHAR: OpStat = OpStat::new();
/// `Framebuffer::scroll_up` — reads the whole framebuffer back. The one
/// operation here that is dominated by VRAM *reads*, which are
/// non-posted: the CPU stalls until the data returns, unlike a write.
pub static FB_SCROLL: OpStat = OpStat::new();
/// `Framebuffer::xor_rect` — the blinking cursor, read-modify-write over
/// one cell, from the PIT ISR at 100 Hz plus once per console write.
pub static FB_CURSOR: OpStat = OpStat::new();
/// `Framebuffer::blit_scaled` — `FBIO_BLIT`, i.e. DOOM/Quake frames.
pub static FB_BLIT: OpStat = OpStat::new();
/// `framebuffer_console::render_bytes` — the whole text path for one
/// write, everything above included. `bytes` is the input byte count, not
/// framebuffer bytes, so its rate is not a memory bandwidth.
pub static FB_RENDER: OpStat = OpStat::new();
/// `framebuffer_console::mirror_to_serial` — one port write per byte of
/// user output, on the hot path, to a UART nothing is listening to on the
/// target machine.
pub static FB_SERIAL_MIRROR: OpStat = OpStat::new();
/// `Framebuffer::flush` — shadow → VRAM copy of the dirty rectangle. In
/// shadow mode this is the only thing that touches VRAM, and it only
/// writes, so its MB/s is the real write bandwidth to the aperture: the
/// number the write-combining phases of `docs/fb/wc-shadow-plan.md` have
/// to move. Zero calls means there is no shadow (direct mode).
pub static FB_FLUSH: OpStat = OpStat::new();

/// Render every framebuffer cost counter, for `/proc/fbinfo`.
pub fn render_fb_report() -> alloc::string::String {
    use alloc::string::String;

    let hz = crate::cpu::tsc::freq_hz();
    let mut out = String::new();
    out.push_str(&FB_FILL_RECT.render("fb_fill_rect", hz));
    out.push_str(&FB_DRAW_CHAR.render("fb_draw_char", hz));
    out.push_str(&FB_SCROLL.render("fb_scroll_up", hz));
    out.push_str(&FB_CURSOR.render("fb_cursor_xor", hz));
    out.push_str(&FB_BLIT.render("fb_blit_scaled", hz));
    out.push_str(&FB_RENDER.render("fb_render_bytes", hz));
    out.push_str(&FB_SERIAL_MIRROR.render("fb_serial_mirror", hz));
    out.push_str(&FB_FLUSH.render("fb_flush", hz));
    out
}

/// Render the current state for `/proc/kdebug`: enabled subsystems (by
/// name, not just the raw mask) plus every counter above.
pub fn render_report() -> alloc::string::String {
    use alloc::format;
    use alloc::string::String;

    let mask = get_mask();
    let mut enabled = String::new();
    for sub in ALL_SUBSYSTEMS {
        if mask & sub.bit != 0 {
            if !enabled.is_empty() { enabled.push(','); }
            enabled.push_str(sub.name);
        }
    }
    if enabled.is_empty() {
        enabled.push_str("(none)");
    }

    format!(
        "trace_mask: {:#x} ({})\n\
         forks_total: {}\n\
         execs_total: {}\n\
         reaps_total: {}\n\
         cow_faults_resolved: {}\n\
         cow_faults_failed: {}\n\
         orphan_blocks_reclaimed: {}\n\
         orphan_inodes_reclaimed: {}\n\
         switches_total: {}\n\
         cow_tracked_frames: {} ({} MiB of RAM)\n\
         usb_keyboards: {}\n\
         usb_key_reports: {}\n\
         usb_keys_dropped: {}\n\
         spurious_irqs: {}\n\
         unexpected_irqs: {} (last line {})\n\
         {}\n\
         {}{}{}{}",
        mask, enabled,
        FORKS_TOTAL.load(Ordering::Relaxed),
        EXECS_TOTAL.load(Ordering::Relaxed),
        REAPS_TOTAL.load(Ordering::Relaxed),
        COW_FAULTS_RESOLVED.load(Ordering::Relaxed),
        COW_FAULTS_FAILED.load(Ordering::Relaxed),
        ORPHAN_BLOCKS_RECLAIMED.load(Ordering::Relaxed),
        ORPHAN_INODES_RECLAIMED.load(Ordering::Relaxed),
        SWITCHES_TOTAL.load(Ordering::Relaxed),
        crate::memory::cow::tracked_frames(),
        (crate::memory::cow::tracked_frames() * 4096) / (1024 * 1024),
        crate::usb::keyboard_count(),
        USB_KEY_REPORTS.load(Ordering::Relaxed),
        USB_KEYS_DROPPED.load(Ordering::Relaxed),
        SPURIOUS_IRQS.load(Ordering::Relaxed),
        UNEXPECTED_IRQS.load(Ordering::Relaxed),
        LAST_UNEXPECTED_IRQ.load(Ordering::Relaxed) as i64,
        match crate::fs::ext2::cache_stats() {
            Some(c) => alloc::format!(
                "ext2_cache: hits={} misses={} device_reads={} device_kib={} passthrough={}",
                c.hits, c.misses, c.device_reads, c.device_sectors / 2, c.passthrough
            ),
            None => alloc::string::String::from("ext2_cache: (no ext2 mount)"),
        },
        SCHEDULER_LOCK.render("scheduler"),
        RAMFS_ENTRIES_LOCK.render("ramfs_entries_lock"),
        TF_REWIND.render(),
        alloc::format!(
            "{}{}{}{}",
            COW_IF_VIOLATIONS_INC_REF.render("cow_if_violations_inc_ref"),
            COW_IF_VIOLATIONS_DEC_REF.render("cow_if_violations_dec_ref"),
            COW_IF_VIOLATIONS_GET_REF.render("cow_if_violations_get_ref"),
            COW_IF_ENABLED_SET_REF.render("cow_if_enabled_set_ref"),
        ),
    )
}

/// Allocation-free counter dump for the panic handler (`panic.rs`) —
/// see its call site for why this can't just call `render_report()`
/// (that one builds a `String` via `format!`, unsafe to do from a panic
/// whose root cause might be heap corruption). Every line here goes
/// straight through `serial_println_raw!`, which formats lazily with no
/// allocation.
pub fn print_panic_snapshot() {
    crate::serial_println_raw!("--- /proc/kdebug snapshot at panic ---");
    crate::serial_println_raw!("  forks_total: {}", FORKS_TOTAL.load(Ordering::Relaxed));
    crate::serial_println_raw!("  execs_total: {}", EXECS_TOTAL.load(Ordering::Relaxed));
    crate::serial_println_raw!("  reaps_total: {}", REAPS_TOTAL.load(Ordering::Relaxed));
    crate::serial_println_raw!("  cow_faults_resolved: {}", COW_FAULTS_RESOLVED.load(Ordering::Relaxed));
    crate::serial_println_raw!("  cow_faults_failed: {}", COW_FAULTS_FAILED.load(Ordering::Relaxed));
    crate::serial_println_raw!("  switches_total: {}", SWITCHES_TOTAL.load(Ordering::Relaxed));
    crate::serial_println_raw!("  spurious_irqs: {}", SPURIOUS_IRQS.load(Ordering::Relaxed));
    crate::serial_println_raw!("  unexpected_irqs: {} (last line {})",
        UNEXPECTED_IRQS.load(Ordering::Relaxed), LAST_UNEXPECTED_IRQ.load(Ordering::Relaxed) as i64);
    // These three lines used to read `diag`'s private fields / call its
    // `print_panic_line` methods directly. They now go through plain,
    // allocation-free accessors — the printed text is byte-for-byte what it
    // was before the extraction, including `ramfs_entries_lock`'s
    // `last_acquirer pid=` (a space, unlike `render()`'s `last_acquirer=pid=`).
    let acq = SCHEDULER_LOCK.acquires();
    let rel = SCHEDULER_LOCK.releases();
    crate::serial_println_raw!("  scheduler_lock: acquires={} releases={} outstanding={}", acq, rel, acq.saturating_sub(rel));
    crate::serial_println_raw!(
        "  {}: acquires={} releases={} outstanding={} last_acquirer pid={} op={}",
        "ramfs_entries_lock",
        RAMFS_ENTRIES_LOCK.acquires(),
        RAMFS_ENTRIES_LOCK.releases(),
        RAMFS_ENTRIES_LOCK.outstanding(),
        RAMFS_ENTRIES_LOCK.last_pid(),
        RAMFS_ENTRIES_LOCK.last_op(),
    );
    crate::serial_println_raw!(
        "  tf_rewind: count={} last_pid={} last_site={} last_seq={}->{}",
        TF_REWIND.count(),
        TF_REWIND.last_pid(),
        TF_REWIND.last_site(),
        TF_REWIND.last_old_seq(),
        TF_REWIND.last_new_seq(),
    );
    crate::serial_println_raw!(
        "  cow_tracked_frames: {} ({} MiB of RAM)",
        crate::memory::cow::tracked_frames(),
        (crate::memory::cow::tracked_frames() * 4096) / (1024 * 1024),
    );
    crate::serial_println_raw!(
        "  cow_if_violations: inc_ref count={} last_line={} | dec_ref count={} last_line={} | get_ref count={} last_line={}",
        COW_IF_VIOLATIONS_INC_REF.count(), COW_IF_VIOLATIONS_INC_REF.last_line(),
        COW_IF_VIOLATIONS_DEC_REF.count(), COW_IF_VIOLATIONS_DEC_REF.last_line(),
        COW_IF_VIOLATIONS_GET_REF.count(), COW_IF_VIOLATIONS_GET_REF.last_line(),
    );
    crate::serial_println_raw!(
        "  cow_if_enabled_set_ref: count={} last_line={} (informational — set_ref does not require IF=0, see memory/cow.rs)",
        COW_IF_ENABLED_SET_REF.count(), COW_IF_ENABLED_SET_REF.last_line(),
    );
}

/// Resolve a subsystem name (e.g. "mm") to its bit, for the `kdebug_ctl`
/// syscall's by-name form. Case-sensitive, matches `Subsystem::name`.
pub fn subsystem_bit_by_name(name: &str) -> Option<u32> {
    ALL_SUBSYSTEMS.iter().find(|s| s.name == name).map(|s| s.bit)
}
