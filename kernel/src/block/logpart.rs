// kernel/src/block/logpart.rs
//
// Copies the kernel log ring (`crate::klog`) onto a raw partition of the
// boot pendrive, `constanos-log`, so the physical machine — which has no
// serial capture — can be rebooted into its own Linux and have the log read
// straight off the stick (`scripts/usb-log.sh read`). No photos of the
// screen, no transcription.
//
// The on-disk format, the per-boot slot choice and the incremental-flush
// arithmetic are `hal::logpart` (pure, host-tested); this file is the
// adapter: find the partition, hold its state, decide *when* to flush.
//
// WHY A RAW PARTITION AND NOT A FILE ON /mnt
// ──────────────────────────────────────────
// Writing ext2 on the stick would put the data partition — and the stick is
// also the boot key — at the mercy of a crash mid-write, with no journal.
// Here there is no metadata at all: a torn flush spoils at most the log
// itself. It also works when the VFS, ext2 or userspace are the broken
// part, which is exactly when a log is needed. And it is safe to write
// before ext2 writing (step 6 of `docs/storage/usb-msc-plan.md`) exists.
//
// WHEN IT FLUSHES
// ───────────────
// * **Periodically, from the idle task** (`periodic`, every
//   `PERIOD_NS`, only if the ring grew). Idle is the one context that is
//   both a normal kernel context (it may wait on the USB controller) and
//   free — it runs precisely when nothing else wants the CPU. Not from the
//   timer ISR: a failing transfer logs through `serial_println!`, whose
//   `SERIAL` lock the interrupted code may hold. The cost of that choice: a
//   user process spinning at 100% CPU starves idle, and with it the
//   periodic flush, until it blocks or dies.
// * **On `sync(2)`** (syscall 162, `kdebug sync`) — the explicit "write it
//   now" before rebooting.
// * **From the panic handler**, best-effort (`on_panic`): `try_lock` on
//   everything, skipped outright if `SERIAL` is held. A panic that fires
//   with the USB controller's lock held loses this last flush; the previous
//   periodic one is still on the stick.
//
// Every flush runs with interrupts off from start to end: a flush is at
// most 129 sectors in two transfers, and making it unpreemptible is what
// makes `STATE` impossible to contend on this single-CPU kernel — the only
// other taker is the panic handler, which only `try_lock`s.
//
// SAFETY OF WRITING RAW SECTORS
// ─────────────────────────────
// Three independent guards stand between a bug here and the neighbouring
// partitions: the partition is looked up by exact name (no fallback);
// sector 0 must hold the marker only `scripts/usb-log.sh init` writes; and
// every write goes through `hal::block::Partition`, which refuses rather
// than clamps any request outside the partition's window.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use hal::block::{BlockDevice, Partition, SECTOR_SIZE};
use hal::logpart::{self as fmt, Header, Reason};

use super::usb::{partition_by_name, UsbBlockDevice};

const RING_BYTES: usize = crate::klog::CAPACITY;
const _: () = assert!(RING_BYTES <= fmt::MAX_RING_BYTES && RING_BYTES % SECTOR_SIZE == 0);

/// Sectors per write — one USB bounce buffer, and within `BlockDevice`'s
/// `u8` count.
const CHUNK_SECTORS: usize = if crate::usb::xhci::MAX_SECTORS < 255 { crate::usb::xhci::MAX_SECTORS } else { 255 };

/// Periodic flush interval. Short enough that a hang loses only the last
/// few seconds; long enough that an idle machine's USB traffic is noise.
const PERIOD_NS: u64 = 5_000_000_000;

/// Consecutive failed periodic flushes after which periodic flushing stops.
/// A dead stick costs up to `BULK_TIMEOUT_MS` (5 s) with interrupts off per
/// attempt; retrying that every period would freeze the machine for good.
const MAX_PERIODIC_FAILURES: u32 = 3;

struct State {
    /// Blocking handle — periodic and `sync` flushes.
    part: Partition,
    /// Gives up on a busy controller instead of waiting — the panic flush.
    panic_part: Partition,
    slot_lba: u32,
    seq: u64,
    flushed_to: u64,
    flushes: u32,
}

static STATE: crate::sync::Mutex<Option<State>> = crate::sync::Mutex::new(None);

/// Staging buffer for ring sectors on their way to the disk. Only touched
/// while `STATE` is held; a static rather than part of `State` so building
/// the state at boot does not move 64 KiB across the boot stack.
struct Scratch(UnsafeCell<[u8; CHUNK_SECTORS * SECTOR_SIZE]>);
unsafe impl Sync for Scratch {}
static SCRATCH: Scratch = Scratch(UnsafeCell::new([0; CHUNK_SECTORS * SECTOR_SIZE]));

// Lock-free mirrors, so the idle task's every-tick check costs two loads.
static READY: AtomicBool = AtomicBool::new(false);
static LAST_FLUSH_NS: AtomicU64 = AtomicU64::new(0);
static FLUSHED_TO: AtomicU64 = AtomicU64::new(0);
static PERIODIC_FAILURES: AtomicU32 = AtomicU32::new(0);

/// Finds and claims the log partition. Called once at boot, after the USB
/// driver; a missing or unformatted partition is logged and otherwise
/// ignored — the kernel runs exactly as before.
pub fn init() {
    match try_init() {
        Ok((seq, slot, slots, first, sectors)) => {
            crate::serial_println!(
                "klog-disk: boot #{} -> slot {}/{} of '{}' (LBA {}, {} sectors)",
                seq, slot, slots, fmt::PARTITION_NAME, first, sectors
            );
            READY.store(true, Ordering::Release);
        }
        Err(e) => crate::serial_println!("klog-disk: not in use: {}", e),
    }
}

fn try_init() -> Result<(u64, u32, u32, u32, u32), &'static str> {
    let (dev, first, sectors) = partition_by_name(fmt::PARTITION_NAME)?;
    let part = Partition::new(Box::new(UsbBlockDevice::new(dev)), first, sectors, false)
        .ok_or("partition beyond 32-bit LBAs")?;
    let panic_part = Partition::new(Box::new(UsbBlockDevice::new_nonblocking(dev)), first, sectors, false)
        .ok_or("partition beyond 32-bit LBAs")?;

    let mut sector = [0u8; SECTOR_SIZE];
    part.read_sectors(0, 1, &mut sector)?;
    if !fmt::is_formatted(&sector) {
        crate::kalert!("klog-disk: '{}' sin formato — scripts/usb-log.sh init", fmt::PARTITION_NAME);
        return Err("partition found but not formatted (run scripts/usb-log.sh init)");
    }

    let slots = fmt::slot_count(sectors as u64);
    let mut headers = Vec::with_capacity(slots as usize);
    for i in 0..slots {
        part.read_sectors(fmt::slot_lba(i) as u32, 1, &mut sector)?;
        headers.push(Header::decode(&sector));
    }
    let (slot, seq) = fmt::choose_slot(&headers).ok_or("partition too small for one slot")?;
    let slot_lba = fmt::slot_lba(slot) as u32;

    // Claim the slot right away with an empty header. Until this lands the
    // slot still describes some older boot, and a flush that died between
    // writing new ring sectors and the new header would leave that old
    // header vouching for half-new data. It also makes a boot that hangs
    // before its first flush visible as a boot with an empty log.
    let claim = Header {
        seq,
        ring_bytes: RING_BYTES as u32,
        write_pos: 0,
        uptime_ns: crate::time::ktime_get(),
        unix_secs: crate::time::now_unix_secs(),
        flushes: 0,
        reason: Reason::Periodic,
    };
    part.write_sectors(slot_lba, 1, &claim.encode())?;

    *STATE.lock() = Some(State { part, panic_part, slot_lba, seq, flushed_to: 0, flushes: 0 });
    Ok((seq, slot, slots, first, sectors))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushError {
    /// No formatted `constanos-log` partition was found at boot.
    NoPartition,
    /// Another flush holds the state (only possible from the panic
    /// handler, which interrupted one).
    Busy,
    Io(&'static str),
}

impl core::fmt::Display for FlushError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            FlushError::NoPartition => f.write_str("no log partition"),
            FlushError::Busy => f.write_str("flush already in progress"),
            FlushError::Io(e) => f.write_str(e),
        }
    }
}

/// Writes whatever the ring gained since the last flush, then the header.
/// Returns the log position now on disk.
pub fn flush(reason: Reason) -> Result<u64, FlushError> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut guard = STATE.try_lock().ok_or(FlushError::Busy)?;
        let st = guard.as_mut().ok_or(FlushError::NoPartition)?;
        let dev: &dyn BlockDevice = if reason == Reason::Panic { &st.panic_part } else { &st.part };

        let to = crate::klog::mark() as u64;
        // SAFETY: `STATE` is held, and it is the only thing that grants
        // access to `SCRATCH`.
        let scratch = unsafe { &mut *SCRATCH.0.get() };
        for &(first, count) in fmt::dirty(st.flushed_to, to, RING_BYTES).runs() {
            let (mut s, end) = (first as usize, (first + count) as usize);
            while s < end {
                let n = (end - s).min(CHUNK_SECTORS);
                let buf = &mut scratch[..n * SECTOR_SIZE];
                crate::klog::copy_raw(s * SECTOR_SIZE, buf);
                dev.write_sectors(st.slot_lba + 1 + s as u32, n as u8, buf).map_err(FlushError::Io)?;
                s += n;
            }
        }

        st.flushes += 1;
        let header = Header {
            seq: st.seq,
            ring_bytes: RING_BYTES as u32,
            write_pos: to,
            uptime_ns: crate::time::ktime_get(),
            unix_secs: crate::time::now_unix_secs(),
            flushes: st.flushes,
            reason,
        };
        dev.write_sectors(st.slot_lba, 1, &header.encode()).map_err(FlushError::Io)?;
        st.flushed_to = to;
        FLUSHED_TO.store(to, Ordering::Relaxed);
        Ok(to)
    })
}

/// The idle task's hook: flushes if the period elapsed and the ring grew.
pub fn periodic() {
    if !READY.load(Ordering::Acquire) {
        return;
    }
    let now = crate::time::ktime_get();
    let last = LAST_FLUSH_NS.load(Ordering::Relaxed);
    if now.wrapping_sub(last) < PERIOD_NS {
        return;
    }
    // Every CPU's idle process calls this: one of them per period flushes.
    if LAST_FLUSH_NS.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        return;
    }
    if crate::klog::mark() as u64 == FLUSHED_TO.load(Ordering::Relaxed) {
        return;
    }
    match flush(Reason::Periodic) {
        Ok(_) => PERIODIC_FAILURES.store(0, Ordering::Relaxed),
        Err(e) => {
            let n = PERIODIC_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
            crate::serial_println!("klog-disk: periodic flush failed ({}), {} in a row", e, n);
            if n >= MAX_PERIODIC_FAILURES {
                READY.store(false, Ordering::Release);
                crate::kalert!("klog-disk: {} volcados fallidos seguidos — desactivado", n);
            }
        }
    }
}

/// Last flush from the panic handler. Never blocks: skipped if the serial
/// lock is held (a failing transfer would log through it), and every lock
/// on the way down is only `try_lock`ed. Reports through the lock-free
/// serial writer only.
pub fn on_panic() {
    if crate::serial::is_locked() {
        crate::serial_println_raw!("  klog-disk: SERIAL held — skipping the panic flush");
        return;
    }
    // Flushed even when periodic flushing gave up: this is the last chance.
    match flush(Reason::Panic) {
        Ok(pos) => crate::serial_println_raw!("  klog-disk: log flushed to the USB stick (pos {})", pos),
        Err(FlushError::NoPartition) => {}
        Err(e) => crate::serial_println_raw!("  klog-disk: panic flush failed: {}", e),
    }
}
