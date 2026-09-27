// kernel/src/interrupts/msi.rs
//
// Vectors for message-signalled interrupts (phase 1 of docs/gpu/gpu-plan.md).
//
// The IDT is a `spin::Once` built before anything else in the boot, so a
// driver cannot install a gate when it finds its device. Instead a fixed
// block of vectors gets one stub each at IDT build time (`register_idt`),
// and each stub dispatches through a table a driver fills later
// (`alloc`). The table is atomics only: the stubs run in ISR context and
// take no lock.
//
// The message itself (address = destination LAPIC, data = vector) is
// written into the device by `pci::enable_msi`. Every MSI is delivered to
// one LAPIC, chosen by the driver: whether the handler's work is global or
// per-CPU is the driver's to state (the GPU's goes to CPU 0 and is global).

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::interrupts::exception::ExceptionStackFrame;
use crate::interrupts::idt::InterruptDescriptorTable;

/// First MSI vector. 0x50–0x5F: above the remapped 8259/ISA block
/// (32–47) and far below the IPI vectors (0xF0–0xF2) and the LAPIC
/// spurious vector (0xFF).
pub const VECTOR_BASE: u8 = 0x50;
pub const VECTORS: usize = 16;

/// A driver's MSI handler. Runs in ISR context with IF=0, on whatever CPU
/// the message targets: the ISR rules of CLAUDE.md apply (no plain locks,
/// no allocation under a non-Irq lock). The EOI is sent after it returns.
pub type Handler = fn(vector: u8);

/// `Handler` pointers as `usize`, 0 = vector free.
static HANDLERS: [AtomicUsize; VECTORS] = [const { AtomicUsize::new(0) }; VECTORS];
/// Interrupts taken per vector (`/proc/kdebug`-style introspection and tests).
static COUNTS: [AtomicU64; VECTORS] = [const { AtomicU64::new(0) }; VECTORS];
/// Interrupts on a vector with no handler (a device left enabled by a
/// driver that has since freed the vector).
static STRAY: AtomicU64 = AtomicU64::new(0);

/// Reserves a vector for `handler`. `None` when all are taken.
pub fn alloc(handler: Handler) -> Option<u8> {
    for (i, slot) in HANDLERS.iter().enumerate() {
        if slot.compare_exchange(0, handler as usize, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            COUNTS[i].store(0, Ordering::Relaxed);
            return Some(VECTOR_BASE + i as u8);
        }
    }
    None
}

/// Releases a vector. The device must already have stopped signalling it
/// (MSI disabled); a late message is counted as stray and acknowledged.
pub fn free(vector: u8) {
    if let Some(i) = index(vector) {
        HANDLERS[i].store(0, Ordering::Release);
    }
}

/// Interrupts taken on `vector` since it was allocated.
pub fn count(vector: u8) -> u64 {
    index(vector).map_or(0, |i| COUNTS[i].load(Ordering::Relaxed))
}

pub fn stray() -> u64 {
    STRAY.load(Ordering::Relaxed)
}

fn index(vector: u8) -> Option<usize> {
    let i = vector.checked_sub(VECTOR_BASE)? as usize;
    (i < VECTORS).then_some(i)
}

fn dispatch(i: usize) {
    let h = HANDLERS[i].load(Ordering::Acquire);
    if h == 0 {
        STRAY.fetch_add(1, Ordering::Relaxed);
    } else {
        COUNTS[i].fetch_add(1, Ordering::Relaxed);
        // SAFETY: only `alloc` stores non-zero values, and it stores a
        // `Handler`.
        let handler: Handler = unsafe { core::mem::transmute::<usize, Handler>(h) };
        handler(VECTOR_BASE + i as u8);
    }
    // MSIs always come through the LAPIC (`pci::enable_msi` refuses
    // without one).
    crate::interrupts::apic::eoi();
}

macro_rules! msi_stubs {
    ($($name:ident => $i:expr),* $(,)?) => {
        $(extern "x86-interrupt" fn $name(_: ExceptionStackFrame) { dispatch($i); })*
        const STUBS: [extern "x86-interrupt" fn(ExceptionStackFrame); VECTORS] = [$($name),*];
    };
}

msi_stubs! {
    msi0 => 0, msi1 => 1, msi2 => 2, msi3 => 3, msi4 => 4, msi5 => 5, msi6 => 6, msi7 => 7,
    msi8 => 8, msi9 => 9, msi10 => 10, msi11 => 11, msi12 => 12, msi13 => 13, msi14 => 14, msi15 => 15,
}

/// Installs the stubs. Called once, from the IDT's build (`init::devices`).
pub fn register_idt(idt: &mut InterruptDescriptorTable) {
    for (i, stub) in STUBS.iter().enumerate() {
        idt.add_handler(VECTOR_BASE + i as u8, *stub);
    }
}
