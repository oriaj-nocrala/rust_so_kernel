// kernel/src/reboot.rs
//
// Restarting (or halting) the machine on request — `reboot(2)`.
//
// "Safe" here means two things. First, nothing that should survive the
// reset is still only in RAM: ext2 writes are synchronous (ATA issues a
// CACHE FLUSH after every write, the block cache is write-through) and the
// USB data partition is mounted read-only, so the one buffered thing left
// is the kernel log ring, which is flushed to the stick's `constanos-log`
// partition before anything else happens. Second, the reset itself does not
// depend on a single mechanism that some machine lacks: the methods are
// tried in turn, each given time to take effect, ending in a triple fault
// that no x86 CPU survives.
//
// Order, and why:
//   1. The FADT's RESET_REG — the firmware's own statement of how this
//      machine resets. Linux's default first choice too.
//      Not necessarily a chipset register: the AM4/Ryzen target's points at
//      the SMI command port (0xB2 <- 0xBE), handing the reset to firmware.
//   2. Port 0xCF9 (the PCH/FCH reset control register) — covers firmware
//      that does not set RESET_REG_SUP, or whose RESET_REG does nothing.
//   3. The 8042's pulse-reset command (0xFE) — only if a controller
//      answers; the target Ryzen has none, and writing to an unanswered
//      port is merely useless, but skipping it keeps the log honest.
//   4. Triple fault — an empty IDT and a breakpoint.

use core::arch::asm;

use hal::acpi::ResetSpace;
use hal::PortIo;
use x86_64::instructions::port::Port;

use crate::hal::X86PortIo;
use crate::serial_println;

/// Busy-waits roughly `us` microseconds with interrupts off: a write to the
/// POST-code port 0x80 takes ~1 µs on every PC, which is why Linux uses it
/// for `io_delay`. No clocksource needed — the jiffies fallback would not
/// advance with IF=0.
fn io_delay_us(us: u32) {
    for _ in 0..us {
        unsafe { Port::<u8>::new(0x80).write(0) };
    }
}

/// How long each method gets before the next one is tried.
const METHOD_WAIT_US: u32 = 500_000;

/// Everything both `restart` and `halt` do before touching the hardware:
/// announce, then flush the log to the stick.
fn prepare(what: &str) {
    serial_println!("reboot: {} requested — flushing the log", what);
    match crate::block::logpart::flush(hal::logpart::Reason::Reboot) {
        Ok(pos) => serial_println!("reboot: log flushed to the USB stick (pos {})", pos),
        Err(crate::block::logpart::FlushError::NoPartition) => {}
        Err(e) => serial_println!("reboot: log flush failed: {}", e),
    }
}

/// Logs which method is about to be tried and gets that line onto the
/// stick *before* trying it. A method that works leaves no chance to write
/// anything afterwards, so the last `trying` line in the stick's log is the
/// one that reset the machine — the only way to learn that on hardware
/// with no serial capture. Incremental, so each costs a couple of sectors.
/// Failures are ignored: `prepare` already reported whether the stick
/// works.
fn announce(args: core::fmt::Arguments) {
    serial_println!("reboot: trying {}", args);
    let _ = crate::block::logpart::flush(hal::logpart::Reason::Reboot);
}

/// Resets the machine. Never returns.
pub fn restart() -> ! {
    prepare("restart");
    crate::kalert!("Reiniciando...");
    x86_64::instructions::interrupts::disable();

    if let Some(reg) = crate::acpi::reset_reg() {
        match reg.space {
            ResetSpace::Io(port) => {
                announce(format_args!("the ACPI reset register (port {:#x} <- {:#04x})", port, reg.value));
                X86PortIo.outb(port, reg.value);
                io_delay_us(METHOD_WAIT_US);
            }
            ResetSpace::PciConfig { device, function, offset } => {
                announce(format_args!(
                    "the ACPI reset register (pci 00:{:02x}.{} +{:#x} <- {:#04x})",
                    device, function, offset, reg.value
                ));
                crate::pci::config_write8(device, function, offset, reg.value);
                io_delay_us(METHOD_WAIT_US);
            }
            // Not mapped anywhere this early-ending path could rely on;
            // 0xCF9 below is what firmware puts here in practice.
            ResetSpace::Memory(addr) => {
                serial_println!("reboot: ACPI reset register in memory space ({:#x}) — skipped", addr);
            }
        }
    }

    // Port 0xCF9, Linux's `BOOT_CF9_FORCE` sequence: select a system
    // reset (bit 1) first, then set the reset bit (bit 2) with "full
    // reset" (bit 3) so the platform power-cycles rather than doing a CPU-
    // only warm reset.
    announce(format_args!("port 0xCF9"));
    let cf9 = X86PortIo.inb(0xCF9) & !0x06;
    X86PortIo.outb(0xCF9, cf9 | 0x02);
    io_delay_us(50);
    X86PortIo.outb(0xCF9, cf9 | 0x0E);
    io_delay_us(METHOD_WAIT_US);

    if hal::i8042::controller_present(&X86PortIo) {
        announce(format_args!("the 8042 pulse-reset"));
        // Wait (bounded) for the input buffer to drain before the command.
        for _ in 0..10_000 {
            if X86PortIo.inb(0x64) & 0x02 == 0 {
                break;
            }
            io_delay_us(10);
        }
        X86PortIo.outb(0x64, 0xFE);
        io_delay_us(METHOD_WAIT_US);
    }

    announce(format_args!("a triple fault (every other reset method failed)"));
    triple_fault()
}

/// Stops the machine for good, after the same flush `restart` does: the
/// "safe to press the power button now" state. Also what `POWER_OFF` does,
/// since there is no AML interpreter here to find the S5 sleep values —
/// the same fallback Linux takes when no `pm_power_off` is registered.
pub fn halt() -> ! {
    prepare("halt");
    crate::kalert!("Sistema detenido. Ya puedes apagar el equipo.");
    x86_64::instructions::interrupts::disable();
    loop {
        x86_64::instructions::hlt();
    }
}

/// Loads a zero-length IDT and raises an exception: delivering it faults,
/// delivering the double fault faults again, and the CPU shuts down, which
/// the chipset turns into a reset.
fn triple_fault() -> ! {
    let empty = x86_64::structures::DescriptorTablePointer {
        limit: 0,
        base: x86_64::VirtAddr::zero(),
    };
    unsafe {
        x86_64::instructions::tables::lidt(&empty);
        asm!("int3", options(noreturn));
    }
}
