// kernel/src/interrupts/apic.rs
//
// Local APIC + I/O APIC — stage 1 of docs/smp/smp-plan.md: the 8259 PIC and
// the PIT stop delivering interrupts, the LAPIC timer drives scheduling at
// the same 100 Hz, and ISA lines (keyboard, COM1, mouse) arrive through the
// I/O APIC. Still one CPU; what this buys is the per-CPU timer and EOI that
// every later stage needs.
//
// Hardware half only. Register layout, encodings, calibration arithmetic and
// the ISA → GSI routing (overrides, polarity, trigger) are pure and
// host-tested in `hal::apic`.
//
// Best-effort like every other hardware step at boot: `init` either switches
// completely or returns an error with the PIC still delivering, so a machine
// with an unusable MADT keeps booting exactly as before. `/proc/kdebug`'s
// `irq_controller:` line says which one is live and why.
//
// xAPIC or x2APIC: whichever the firmware left on. Turning x2APIC back off
// needs the APIC disabled entirely, so a firmware that enabled it decides for
// us; one that didn't gets the MMIO window, which is enough for ≤255 CPUs.

use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};

use alloc::vec::Vec;
use hal::apic::{self, lapic, ioapic, IsaRoute, Polarity, Redirection, TimerMode, Trigger};
use x86_64::registers::model_specific::Msr;
use x86_64::PhysAddr;

use crate::allocator::KernelIrq;
use diag::IrqMutex;

/// Vector of the LAPIC timer — the same one IRQ0 had under the PIC, so
/// `timer_interrupt_entry` serves both and the fallback needs no second IDT.
pub const TIMER_VECTOR: u8 = 32;
/// The LAPIC's spurious-interrupt vector. Low nibble all ones, as older
/// LAPICs hardwire those bits; it must never be EOI'd.
pub const SPURIOUS_VECTOR: u8 = 0xFF;
/// ISA line n is delivered on this vector + n — the PIC's layout, kept so
/// the existing IDT entries (33 keyboard, 36 COM1, 44 mouse) serve both.
const ISA_VECTOR_BASE: u8 = super::pic::PIC1_OFFSET;

const TIMER_HZ: u32 = 100; // same rate the PIT ran at; cursor blink, USB poll and quanta assume it
const TIMER_DIVISOR: u32 = 16;
/// Calibration window, in TSC time.
const CALIBRATION_NS: u64 = 10_000_000;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static X2APIC: AtomicBool = AtomicBool::new(false);
static LAPIC_VIRT: AtomicU64 = AtomicU64::new(0);
/// ISA lines routed through the I/O APIC (bit n = IRQ n).
static ROUTED: AtomicU16 = AtomicU16::new(0);

/// Every I/O APIC's mapped window plus its GSI range. The select/window
/// register pair is a two-step access, so it takes a real lock.
struct IoApicWin {
    virt: u64,
    gsi_base: u32,
    pins: u32,
}
static IOAPICS: IrqMutex<Vec<IoApicWin>, KernelIrq> = IrqMutex::new(Vec::new());

#[derive(Clone, Copy)]
struct Status {
    lapic_id: u32,
    timer_count: u32,
    timer_input_hz: u64,
}
static STATUS: spin::Once<Status> = spin::Once::new();
static FALLBACK_REASON: spin::Once<&'static str> = spin::Once::new();

/// Is the APIC delivering interrupts (rather than the 8259)?
#[inline]
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

// ── Local APIC register access ──────────────────────────────────────────

fn lapic_read(reg: u32) -> u32 {
    if X2APIC.load(Ordering::Relaxed) {
        // SAFETY: x2APIC MSRs exist whenever IA32_APIC_BASE says x2APIC is on.
        unsafe { Msr::new(apic::x2apic_msr(reg)).read() as u32 }
    } else {
        let base = LAPIC_VIRT.load(Ordering::Relaxed);
        // SAFETY: `base` is the uncached mapping of the LAPIC's 4 KiB window.
        unsafe { core::ptr::read_volatile((base + reg as u64) as *const u32) }
    }
}

fn lapic_write(reg: u32, value: u32) {
    if X2APIC.load(Ordering::Relaxed) {
        // SAFETY: as in `lapic_read`.
        unsafe { Msr::new(apic::x2apic_msr(reg)).write(value as u64) }
    } else {
        let base = LAPIC_VIRT.load(Ordering::Relaxed);
        // SAFETY: as in `lapic_read`.
        unsafe { core::ptr::write_volatile((base + reg as u64) as *mut u32, value) }
    }
}

/// End of interrupt for whatever the LAPIC has in service.
#[inline]
pub fn eoi() {
    lapic_write(lapic::EOI, 0);
}

/// Did the LAPIC deliver `vector` (its ISR bit is set)? Tells an I/O APIC
/// interrupt, which needs a LAPIC EOI, from an 8259 leftover, which must
/// not get one — an EOI with nothing of ours in service would end whatever
/// else is.
pub fn in_service(vector: u8) -> bool {
    let (reg, bit) = apic::isr_bit(vector);
    lapic_read(reg) & (1 << bit) != 0
}

// ── I/O APIC register access ────────────────────────────────────────────

fn ioapic_read(virt: u64, reg: u32) -> u32 {
    // SAFETY: `virt` is an uncached mapping of an I/O APIC's window; the
    // caller holds `IOAPICS`, so select + window can't interleave.
    unsafe {
        core::ptr::write_volatile((virt + ioapic::IOREGSEL) as *mut u32, reg);
        core::ptr::read_volatile((virt + ioapic::IOWIN) as *const u32)
    }
}

fn ioapic_write(virt: u64, reg: u32, value: u32) {
    // SAFETY: as in `ioapic_read`.
    unsafe {
        core::ptr::write_volatile((virt + ioapic::IOREGSEL) as *mut u32, reg);
        core::ptr::write_volatile((virt + ioapic::IOWIN) as *mut u32, value);
    }
}

/// Writes redirection entry `pin`. The high dword (destination) first while
/// the entry is masked, then the low dword that may unmask it — so the pin
/// is never live with a half-written destination.
fn write_redirection(virt: u64, pin: u32, entry: u64) {
    let reg = ioapic::redirection(pin);
    ioapic_write(virt, reg, apic::MASKED_ENTRY as u32);
    ioapic_write(virt, reg + 1, (entry >> 32) as u32);
    ioapic_write(virt, reg, entry as u32);
}

/// Routes ISA `line` to this CPU through the I/O APIC (overrides applied).
/// `false` if no I/O APIC serves its GSI.
fn route_isa(line: u8) -> bool {
    let Some(topo) = crate::acpi::topology() else { return false };
    let route = apic::isa_route(line, &topo.overrides);
    let dest = STATUS.get().map(|s| s.lapic_id).unwrap_or(0) as u8;
    let entry = Redirection {
        vector: ISA_VECTOR_BASE + line,
        polarity: route.polarity,
        trigger: route.trigger,
        masked: false,
        dest,
    }
    .encode();
    let ok = IOAPICS.with(|ios| {
        let ranges: Vec<(u32, u32)> = ios.iter().map(|w| (w.gsi_base, w.pins)).collect();
        match apic::ioapic_for_gsi(&ranges, route.gsi) {
            Some((i, pin)) => {
                write_redirection(ios[i].virt, pin, entry);
                true
            }
            None => false,
        }
    });
    if ok {
        ROUTED.fetch_or(1 << line, Ordering::Relaxed);
    } else {
        crate::serial_println!("apic: no I/O APIC serves ISA IRQ {} (GSI {})", line, route.gsi);
    }
    ok
}

/// Routes an ISA line a driver asked for after the switch. Lines 0 (the
/// LAPIC timer replaces the PIT) and 2 (the 8259 cascade, which in QEMU is
/// the PIT's GSI) are never routed.
pub(super) fn enable_isa(line: u8) {
    if line != 0 && line != 2 {
        route_isa(line);
    }
}

// ── Bring-up ────────────────────────────────────────────────────────────

fn cpu_has_apic() -> bool {
    core::arch::x86_64::__cpuid(1).edx & (1 << 9) != 0
}

/// Measures the LAPIC timer against the TSC and returns the periodic
/// initial count for `TIMER_HZ`, plus the implied input clock for the log.
fn calibrate_timer() -> Result<(u32, u64), &'static str> {
    let tsc_hz = crate::cpu::tsc::freq_hz();
    if tsc_hz == 0 {
        return Err("TSC not calibrated");
    }
    let window = tsc_hz * CALIBRATION_NS / 1_000_000_000;

    lapic_write(lapic::TIMER_DIVIDE, apic::divide_config(TIMER_DIVISOR).unwrap());
    lapic_write(lapic::LVT_TIMER, apic::lvt_timer(TIMER_VECTOR, TimerMode::OneShot, true));
    let t0 = crate::cpu::tsc::read();
    lapic_write(lapic::TIMER_INITIAL, u32::MAX);
    let mut t1 = t0;
    // Bounded by the TSC and a spin count both, like every boot-time wait
    // here: a TSC that stopped would otherwise hang the boot right here.
    for _ in 0..1_000_000_000u64 {
        t1 = crate::cpu::tsc::read();
        if t1.wrapping_sub(t0) >= window {
            break;
        }
        core::hint::spin_loop();
    }
    let remaining = lapic_read(lapic::TIMER_CURRENT);
    lapic_write(lapic::TIMER_INITIAL, 0); // stop

    let ticks = (u32::MAX - remaining) as u64;
    let cycles = t1.wrapping_sub(t0);
    let count = apic::periodic_initial_count(ticks, cycles, tsc_hz, TIMER_HZ).map_err(|e| match e {
        apic::CalibrationError::NoTicks => "LAPIC timer did not count",
        apic::CalibrationError::NoReference => "no TSC reference",
        apic::CalibrationError::OutOfRange => "LAPIC timer rate out of range",
    })?;
    Ok((count, apic::timer_input_hz(ticks, TIMER_DIVISOR, cycles, tsc_hz)))
}

/// Switches interrupt delivery from the 8259 + PIT to the LAPIC timer + I/O
/// APIC. Call once, with interrupts disabled, after `cpu::tsc::init()` (the
/// timer is calibrated against the TSC) and after the drivers that enable
/// ISA lines at boot have run (their lines are re-routed here).
///
/// On error nothing has been switched and the PIC keeps delivering.
pub fn init() {
    match try_init() {
        Ok(()) => crate::serial_println!("apic: {}", render()),
        Err(reason) => {
            FALLBACK_REASON.call_once(|| reason);
            crate::serial_println!("apic: staying on the 8259 PIC: {}", reason);
        }
    }
}

fn try_init() -> Result<(), &'static str> {
    let topo = crate::acpi::topology().ok_or("no MADT")?;
    if topo.io_apics.is_empty() {
        return Err("MADT lists no I/O APIC");
    }
    if !cpu_has_apic() {
        return Err("CPUID says no local APIC");
    }

    // ── Local APIC: find it, map it ─────────────────────────────────────
    let mut base_msr = Msr::new(apic::IA32_APIC_BASE_MSR);
    // SAFETY: IA32_APIC_BASE exists whenever CPUID reports an APIC.
    let raw = unsafe { base_msr.read() };
    let base = apic::ApicBase::decode(raw);
    if !base.enabled {
        // Globally enable it (xAPIC). Firmware normally leaves it on.
        // SAFETY: setting bit 11 only; the base address is unchanged.
        unsafe { base_msr.write(raw | 1 << 11) };
    }
    if base.x2apic {
        X2APIC.store(true, Ordering::Relaxed);
    } else {
        // SAFETY: the LAPIC window is device registers, 4 KiB.
        let virt = unsafe { crate::memory::mmio::map(PhysAddr::new(base.phys), 0x1000) }
            .ok_or("cannot map the local APIC")?;
        LAPIC_VIRT.store(virt.as_u64(), Ordering::Relaxed);
    }
    if topo.local_apic_addr != base.phys && !base.x2apic {
        crate::serial_println!(
            "apic: MADT says LAPIC @ {:#x}, IA32_APIC_BASE says {:#x}; using the MSR",
            topo.local_apic_addr, base.phys
        );
    }

    // ── Timer calibration, with the LAPIC software-enabled ──────────────
    // Before touching the PIC or the I/O APICs, so a failure here leaves
    // nothing to undo but the SVR.
    let old_svr = lapic_read(lapic::SVR);
    lapic_write(lapic::SVR, SPURIOUS_VECTOR as u32 | lapic::SVR_ENABLE);
    let (count, input_hz) = match calibrate_timer() {
        Ok(v) => v,
        Err(e) => {
            lapic_write(lapic::SVR, old_svr);
            return Err(e);
        }
    };
    let lapic_id = apic::lapic_id(lapic_read(lapic::ID), base.x2apic);
    if lapic_id > 0xFF {
        lapic_write(lapic::SVR, old_svr);
        return Err("LAPIC ID above 255 needs x2APIC destinations");
    }

    // ── I/O APICs: map, mask every pin ──────────────────────────────────
    let mut wins = Vec::new();
    for io in &topo.io_apics {
        // SAFETY: an I/O APIC's register window (IOREGSEL + IOWIN, 0x20 bytes).
        let Some(virt) = (unsafe { crate::memory::mmio::map(PhysAddr::new(io.address as u64), 0x20) }) else {
            lapic_write(lapic::SVR, old_svr);
            return Err("cannot map an I/O APIC");
        };
        let virt = virt.as_u64();
        let pins = ioapic::entry_count(ioapic_read(virt, ioapic::REG_VERSION));
        for pin in 0..pins {
            write_redirection(virt, pin, apic::MASKED_ENTRY);
        }
        wins.push(IoApicWin { virt, gsi_base: io.gsi_base, pins });
    }
    IOAPICS.with(|ios| *ios = wins);
    STATUS.call_once(|| Status { lapic_id, timer_count: count, timer_input_hz: input_hz });

    // ── Switch ──────────────────────────────────────────────────────────
    // IF=0 throughout, so the order below only has to leave the hardware
    // consistent by the first `sti`, not at every step.
    super::pic::mask_all();
    lapic_write(lapic::TPR, 0);
    // LINT0 carries the 8259's ExtINT in virtual-wire mode: masked, so the
    // PIC can't reach this CPU even if something unmasks it later.
    lapic_write(lapic::LVT_LINT0, lapic::LVT_MASKED);
    lapic_write(lapic::LVT_ERROR, lapic::LVT_MASKED);
    lapic_write(lapic::ESR, 0);

    let wanted = super::enabled_isa_lines();
    for line in 0..16u8 {
        if wanted & (1 << line) != 0 {
            enable_isa(line);
        }
    }
    drain_edge_sources();

    lapic_write(lapic::TIMER_DIVIDE, apic::divide_config(TIMER_DIVISOR).unwrap());
    lapic_write(lapic::LVT_TIMER, apic::lvt_timer(TIMER_VECTOR, TimerMode::Periodic, false));
    lapic_write(lapic::TIMER_INITIAL, count);
    ACTIVE.store(true, Ordering::Relaxed);
    Ok(())
}

/// Empties the devices behind edge-triggered ISA lines. A device whose line
/// was already asserted when its pin got unmasked (a key pressed during
/// boot, a byte on COM1) produces no edge the I/O APIC can see, and keeps
/// the line high until it is read — so it would never interrupt again.
/// Reading them here costs at most what was typed during boot.
fn drain_edge_sources() {
    use x86_64::instructions::port::Port;
    // SAFETY: 8042 status/data and 16550 LSR/RBR reads; a read only
    // consumes the byte being discarded.
    unsafe {
        let mut status: Port<u8> = Port::new(0x64);
        let mut data: Port<u8> = Port::new(0x60);
        for _ in 0..32 {
            let s = status.read();
            if s == 0xFF || s & 1 == 0 {
                break; // no controller, or output buffer empty
            }
            data.read();
        }
        let mut lsr: Port<u8> = Port::new(0x3FD);
        let mut rbr: Port<u8> = Port::new(0x3F8);
        for _ in 0..64 {
            let s = lsr.read();
            if s == 0xFF || s & 1 == 0 {
                break;
            }
            rbr.read();
        }
    }
}

/// `/proc/kdebug`'s `irq_controller:` line (without the key).
pub fn render() -> alloc::string::String {
    use alloc::format;
    use core::fmt::Write;

    if !active() {
        return format!("8259 pic + pit ({})", FALLBACK_REASON.get().copied().unwrap_or("apic not tried"));
    }
    let st = STATUS.get().copied().unwrap_or(Status { lapic_id: 0, timer_count: 0, timer_input_hz: 0 });
    let mut s = format!(
        "{} lapic id {}, timer {} Hz: count {} div {} (input {} MHz), ioapics",
        if X2APIC.load(Ordering::Relaxed) { "x2apic" } else { "xapic" },
        st.lapic_id, TIMER_HZ, st.timer_count, TIMER_DIVISOR, st.timer_input_hz / 1_000_000,
    );
    IOAPICS.with(|ios| {
        for w in ios.iter() {
            let _ = write!(s, " [gsi {}..{}]", w.gsi_base, w.gsi_base + w.pins - 1);
        }
    });
    s.push_str(", isa");
    let routed = ROUTED.load(Ordering::Relaxed);
    let overrides = crate::acpi::topology().map(|t| &t.overrides[..]).unwrap_or(&[]);
    for line in 0..16u8 {
        if routed & (1 << line) != 0 {
            let IsaRoute { gsi, polarity, trigger } = apic::isa_route(line, overrides);
            let _ = write!(
                s,
                " {}->gsi{}{}{}",
                line,
                gsi,
                if trigger == Trigger::Level { "/level" } else { "" },
                if polarity == Polarity::ActiveLow { "/low" } else { "" },
            );
        }
    }
    s
}
