// kernel/src/klog.rs
//
// The kernel message ring — everything `serial_println!` and
// `serial_println_raw!` emit, kept in memory so it can be read back after
// the fact instead of only as it streams past.
//
// WHY THIS EXISTS
// ───────────────
// Every diagnostic this kernel prints goes to COM1, and on the physical
// AM4/Ryzen machine it is brought up on there is no serial capture at all.
// The screen is the only output, and the boot messages that *do* reach it
// were being destroyed twice over: `FramebufferConsole::new()` clears the
// whole screen the first time a user process opens `/dev/fb` (wiping
// anything the kernel drew during boot — including a `kalert!` notice), and
// whatever survived that scrolled away under the shell's own output
// moments later. A driver could report exactly what went wrong and the
// report would be unreadable on the one machine it was written for.
//
// So: keep the bytes. `cat /proc/dmesg` reads them back once there is a
// keyboard, and `init::boot` can render the tail straight to the screen
// when there is not (see `init::mod`'s no-input hold).
//
// LOCK-FREE ON PURPOSE
// ────────────────────
// `push` is reachable from every context this kernel has — the timer ISR,
// the page-fault handler, the allocator, the panic handler — so it takes no
// lock at all. A single `fetch_add` reserves a byte range and the writer
// fills it. Two concurrent writers can interleave their bytes, which is
// exactly the trade-off `RawSerialWriter` already documents and accepts for
// the same reason: a debug log that occasionally interleaves is worth far
// more than one that can deadlock the machine it is debugging.
//
// The buffer is a fixed BSS array, never allocated: it must work before
// `memory::init_core` has run (the bootloader's own log lines already pass
// through here) and inside the panic handler, which is verified to touch
// neither the heap nor `BUDDY`.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Ring capacity. A full boot to the shell prompt measures ~14 KiB of
/// serial output, so 64 KiB holds the entire boot plus a comfortable amount
/// of runtime before the oldest lines start falling off the back.
pub const CAPACITY: usize = 64 * 1024;

struct Ring(UnsafeCell<[u8; CAPACITY]>);

// SAFETY: every access goes through `WRITE_POS`-derived indices and
// volatile byte accesses; see the module comment on why torn/interleaved
// content is an accepted trade-off rather than a bug here.
unsafe impl Sync for Ring {}

static BUF: Ring = Ring(UnsafeCell::new([0; CAPACITY]));

/// Total bytes ever written, monotonic — not an index. The modulo into the
/// array is taken per byte, so this doubles as the "did it wrap yet?" flag
/// (`> CAPACITY`) and as a stable marker a caller can take now and compare
/// against later (see [`mark`]).
static WRITE_POS: AtomicUsize = AtomicUsize::new(0);

/// Appends bytes to the ring. Callable from any context; never blocks,
/// never allocates, never panics.
pub fn push(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let start = WRITE_POS.fetch_add(bytes.len(), Ordering::Relaxed);
    let buf = BUF.0.get() as *mut u8;
    for (i, &b) in bytes.iter().enumerate() {
        // SAFETY: the index is taken modulo the array's own length, so it
        // is always in bounds; concurrent writers may overlap, which is
        // accounted for above.
        unsafe { core::ptr::write_volatile(buf.add((start + i) % CAPACITY), b) };
    }
}

/// Total bytes written so far — a marker that can be handed back to
/// [`copy_since`] to read only what was logged after this point.
pub fn mark() -> usize {
    WRITE_POS.load(Ordering::Relaxed)
}

/// Oldest byte position still held in the ring.
fn oldest() -> usize {
    mark().saturating_sub(CAPACITY)
}

/// Copies the most recent bytes into `out`, returning how many were
/// written. Takes the tail when `out` is smaller than what the ring holds —
/// the recent end is the useful one.
pub fn copy_tail(out: &mut [u8]) -> usize {
    let end = mark();
    let want = out.len().min(end - oldest());
    copy_range(end - want, end, out)
}

/// Copies everything logged since `from` (a value from [`mark`]), clamped
/// to what the ring still holds. Returns how many bytes were written.
pub fn copy_since(from: usize, out: &mut [u8]) -> usize {
    let end = mark();
    let start = from.max(oldest()).min(end);
    let want = out.len().min(end - start);
    // A range longer than `out` keeps its tail, matching `copy_tail`.
    copy_range(end - want, end, out)
}

fn copy_range(start: usize, end: usize, out: &mut [u8]) -> usize {
    let buf = BUF.0.get() as *const u8;
    let n = (end - start).min(out.len());
    for (i, slot) in out[..n].iter_mut().enumerate() {
        // SAFETY: same bounds argument as `push`.
        *slot = unsafe { core::ptr::read_volatile(buf.add((start + i) % CAPACITY)) };
    }
    n
}

/// Copies the ring's raw array — by array index, not by log position —
/// starting at `offset`. This is what `block::logpart` writes to disk: the
/// ring as it sits in memory, sector for sector, so a flush only has to
/// rewrite the sectors that changed (see `hal::logpart`). Anything past
/// the end of the array is left untouched rather than panicking — the
/// panic handler is one of the callers.
pub fn copy_raw(offset: usize, out: &mut [u8]) {
    let n = out.len().min(CAPACITY.saturating_sub(offset));
    let buf = BUF.0.get() as *const u8;
    for (i, slot) in out[..n].iter_mut().enumerate() {
        // SAFETY: `offset + i < CAPACITY` by the clamp above.
        *slot = unsafe { core::ptr::read_volatile(buf.add(offset + i)) };
    }
}

/// How many bytes the ring currently holds.
pub fn len() -> usize {
    mark().min(CAPACITY)
}

/// Whether the ring has wrapped, i.e. the earliest messages are gone. What
/// `/proc/dmesg` reports so a reader knows the log is not from byte zero.
pub fn wrapped() -> bool {
    mark() > CAPACITY
}

/// The whole ring as an owned `String`, oldest first, for `/proc/dmesg`.
/// Allocates — callers must be in a context where the heap is usable (the
/// procfs `open()` path is; the panic handler and `push` itself are not).
pub fn render() -> alloc::string::String {
    use alloc::string::String;
    use alloc::vec;

    let n = len();
    let mut bytes = vec![0u8; n];
    let copied = copy_tail(&mut bytes);
    bytes.truncate(copied);

    let mut out = String::new();
    if wrapped() {
        out.push_str("[klog wrapped — oldest messages dropped]\n");
    }
    // Non-UTF-8 can only get here through a corrupted write; replace rather
    // than lose the whole log to one bad byte.
    out.push_str(&alloc::string::String::from_utf8_lossy(&bytes));
    out
}

// ── On-screen dump (the no-keyboard path) ────────────────────────────────────

/// Scratch space for [`dump_to_screen`]. A static rather than a stack array
/// because this runs on the bootloader's own boot stack, where 16 KiB is
/// not obviously free, and because it is used exactly once per boot.
struct Scratch(UnsafeCell<[u8; SCRATCH_LEN]>);
unsafe impl Sync for Scratch {}
const SCRATCH_LEN: usize = 16 * 1024;
static SCRATCH: Scratch = Scratch(UnsafeCell::new([0; SCRATCH_LEN]));
static SCRATCH_BUSY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Renders buffered log lines onto the framebuffer console.
///
/// `needles` filters: a line is kept if it contains any of them; an empty
/// list keeps everything. The filter is the whole reason this is useful —
/// a boot produces ~300 lines and a 1024×768 screen holds ~80, so an
/// unfiltered dump would scroll the interesting part off before anyone
/// could read it. Matching on substrings is crude, but the alternative
/// (log levels/facilities threaded through every existing call site) is a
/// far larger change for a diagnostic that has exactly one caller.
///
/// Best-effort in the same sense as everything else drawn from the kernel:
/// if the console locks are held it prints nothing rather than blocking.
pub fn dump_to_screen(needles: &[&str]) {
    use core::sync::atomic::Ordering as O;

    if SCRATCH_BUSY.swap(true, O::SeqCst) {
        return; // already dumping — never reentrant, but cheap to guarantee
    }

    // SAFETY: the swap above makes this the only live borrow.
    let scratch = unsafe { &mut *SCRATCH.0.get() };
    let n = copy_tail(scratch);

    for line in scratch[..n].split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let keep = needles.is_empty()
            || needles.iter().any(|needle| contains(line, needle.as_bytes()));
        if keep {
            crate::drivers::framebuffer_console::kernel_write_bytes(line);
            crate::drivers::framebuffer_console::kernel_write_bytes(b"\n");
        }
    }

    SCRATCH_BUSY.store(false, O::SeqCst);
}

/// Substring search — `[u8]` has no `contains` for slices, and pulling in a
/// real searcher for a handful of short needles per line would be silly.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return needle.is_empty();
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}
