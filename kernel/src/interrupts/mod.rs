pub mod apic;
pub mod idt;
pub mod pic;
pub mod exception;

use core::sync::atomic::{AtomicU16, Ordering};

/// ISA lines drivers have asked for (bit n = IRQ n), whichever controller
/// was live when they asked — `apic::init` re-routes these when it takes
/// over from the 8259.
static ENABLED_ISA: AtomicU16 = AtomicU16::new(0);

pub(crate) fn enabled_isa_lines() -> u16 {
    ENABLED_ISA.load(Ordering::Relaxed)
}

/// Unmasks ISA IRQ `line` on whichever controller is delivering: the 8259
/// (plus its cascade for a slave line) or the I/O APIC. Drivers call this,
/// never `pic::enable_irq`, so a line enabled before the APIC switch is not
/// lost by it.
pub fn enable_isa_irq(line: u8) {
    ENABLED_ISA.fetch_or(1 << line, Ordering::Relaxed);
    if apic::active() {
        apic::enable_isa(line);
    } else {
        if line >= 8 {
            pic::enable_irq(2); // the master's input from the slave
        }
        pic::enable_irq(line);
    }
}

/// End of interrupt for `vector`, on whichever controller delivered it.
#[inline]
pub fn eoi(vector: u8) {
    if apic::active() {
        apic::eoi();
    } else {
        pic::end_of_interrupt(vector);
    }
}
