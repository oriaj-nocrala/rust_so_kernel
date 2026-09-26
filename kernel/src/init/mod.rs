// kernel/src/init/mod.rs
//
// Boot orchestration — calls sub-modules in the exact order
// the original kernel_main did.

pub mod devices;
pub mod memory;
pub mod processes;
#[cfg(test)]
pub mod test_support;

use bootloader_api::BootInfo;
use x86_64::VirtAddr;

use crate::{
    framebuffer::{Framebuffer, init_global_framebuffer},
    process,
    serial_println,
};

pub fn boot(boot_info: &'static mut BootInfo) -> ! {
    devices::init_idt();
    // `vfs`'s locks answer TLB shootdowns while spinning, like the
    // kernel's own (`crate::sync`); `vfs` can't name `memory::tlb` itself.
    vfs::lock::set_relax_hook(|| {
        crate::memory::tlb::service_pending();
    });
    // ...and stamp `ramfs`'s file times with the wall clock (the boot-time
    // RTC reading plus uptime; uptime alone until `time::init` has run).
    vfs::clock::set_clock(|| crate::time::now_unix_secs());

    // ── Framebuffer setup ──────────────────────────────────────────
    // Stays here because buffer_mut() requires the &'static mut
    // lifetime that flows from boot_info.  Moving this to a function
    // would require either an unsafe transmute or a &'static mut
    // FrameBuffer parameter — both worse than 7 lines inline.
    let fb = boot_info.framebuffer.as_mut().expect("No framebuffer");
    let info = fb.info();
    let buffer = fb.buffer_mut();

    let framebuffer = Framebuffer::new(
        buffer,
        info.width as usize,
        info.height as usize,
        info.stride as usize,
        info.bytes_per_pixel as usize,
    );

    init_global_framebuffer(framebuffer);
    // Console font size from the screen height; before any text is drawn.
    crate::drivers::framebuffer_console::init_font();

    // ── Memory subsystem ───────────────────────────────────────────
    let phys_mem_offset = VirtAddr::new(
        boot_info.physical_memory_offset.into_option().unwrap()
    );

    memory::init_core(phys_mem_offset, &boot_info.memory_regions);

    // Allocate and zero-fill the shared zero frame (used by the zero-page trick).
    unsafe { crate::memory::cow::init_zero_frame(); }

    memory::test_allocators();

    // ── PAT: make entry 1 write-combining ──────────────────────────
    // Before anything can map `PWT`-only and before the first process
    // (every address space is cloned from the kernel's, so checking this
    // one table covers them all). No mapping changes type here; phase 3
    // of `docs/fb/wc-shadow-plan.md` is what points the framebuffer at it.
    serial_println!("PAT: {}", crate::memory::memtype::program_pat());

    // ── Framebuffer: write-combining ───────────────────────────────
    // Points the aperture at the PAT entry just made WC. Phase 3 of the
    // same plan; declines, and says why, if the PAT step did not happen.
    serial_println!("framebuffer: {}", crate::framebuffer::map_write_combining());

    // ── Framebuffer RAM shadow ─────────────────────────────────────
    // Needs the heap, so not before `init_core`; before the boot screen so
    // that is drawn through the shadow too. Best-effort: on failure the
    // console stays in direct-to-VRAM mode, correct but slow on real
    // hardware (a scroll there reads VRAM back at ~4 MB/s). See
    // `docs/fb/wc-shadow-plan.md`, phase 1.
    if crate::framebuffer::attach_shadow() {
        serial_println!("framebuffer: RAM shadow attached");
    } else {
        serial_println!("framebuffer: no RAM shadow, drawing straight to VRAM");
    }

    // ── Hardware watchdog (AMD FCH TCO) ────────────────────────────
    // Armed on every boot as early as MMIO can be mapped, so a hang in any
    // driver below still resets the machine during an unattended run;
    // `watchdog::settle()` disarms it after `/mnt` if there is no job.
    crate::watchdog::arm_early();

    // ── ACPI tables ────────────────────────────────────────────────
    // Best-effort, parse-only (bounded, never hangs boot) — see
    // `acpi::AcpiDriver`. Does NOT touch the existing 8259 PIC/IDT
    // interrupt setup; only extracts interrupt topology (Local APIC, I/O
    // APICs, CPUs, interrupt source overrides) for later use /
    // introspection (/proc/acpi). Needs physical_memory_offset, already up
    // from memory::init_core above. Run through the new best-effort driver
    // registry (`hal::run_all`) as the pilot for that pattern — see the HAL
    // refactor; other drivers (mouse, ac97, ...) still init directly below
    // and migrate onto this incrementally.
    let mut acpi_driver = crate::acpi::AcpiDriver::new(boot_info.rsdp_addr.into_option());
    crate::hal::run_all(&mut [&mut acpi_driver]);

    // ── Boot screen ────────────────────────────────────────────────
    devices::draw_boot_screen();

    // ── Hardware interrupts ────────────────────────────────────────
    devices::init_hardware_interrupts();

    // ── PS/2 mouse ──────────────────────────────────────────────────
    // Best-effort (bounded polls, never hangs boot) — see
    // mouse::MouseDriver. Migrated onto the `hal` seam pattern
    // (`hal::mouse`, PortIo-generic 8042 enable sequence + pure packet
    // decoder), same registry ACPI/ac97 were piloted through.
    let mut mouse_driver = crate::mouse::MouseDriver::new();
    crate::hal::run_all(&mut [&mut mouse_driver]);

    // ── AC97 audio ──────────────────────────────────────────────────
    // Best-effort (bounded polls, never hangs boot) — see ac97::Ac97Driver.
    // Needs phys_alloc/physical_memory_offset, both already up from
    // memory::init_core above. Migrated onto the `hal` seam pattern
    // (`hal::ac97`, PortIo-generic protocol + pure ring state machine),
    // same registry ACPI was piloted through.
    let mut ac97_driver = crate::ac97::Ac97Driver::new();
    crate::hal::run_all(&mut [&mut ac97_driver]);

    // ── TSC calibration ────────────────────────────────────────────
    // PIT is now running; interrupts still masked — safe to busy-poll.
    crate::cpu::tsc::init();
    crate::cpu::freq::init();
    crate::cpu::temp::init();
    serial_println!("TSC: {} MHz", crate::cpu::tsc::freq_hz() / 1_000_000);

    // ── LAPIC + I/O APIC ───────────────────────────────────────────
    // Retires the 8259 + PIT: the LAPIC timer (calibrated against the TSC
    // just above) takes over the 100 Hz tick, and the ISA lines enabled so
    // far (keyboard, COM1, mouse) move to the I/O APIC. Best-effort: on any
    // failure the PIC keeps delivering. Stage 1 of `docs/smp/smp-plan.md`.
    crate::interrupts::apic::init();

    // ── Per-CPU state ──────────────────────────────────────────────
    // GDT + this CPU's TSS slot, IDT, GS, syscall MSRs, PAT, SSE, LAPIC +
    // timer — everything a CPU holds for itself, through the same call every
    // AP will make (stage 3 of `docs/smp/smp-plan.md`), then read back from
    // the hardware. Right after the APIC, the last global decision it
    // applies; before USB and the VFS, so a double fault from here on lands
    // on this CPU's own IST stack.
    if let Err((step, why)) = crate::cpu::init_this_cpu(0) {
        panic!("BSP per-CPU init: step `{}` failed: {}", step, why);
    }
    crate::cpu::percpu::measure_cpu_id_cost();

    // ── Application processors ─────────────────────────────────────
    // Stage 4 of `docs/smp/smp-plan.md`: every AP in the MADT is woken,
    // runs `init_this_cpu` like the BSP just did, and parks in `hlt` —
    // no timer, no processes, nothing routed to it. Bounded: an AP that
    // does not answer is logged and left out. Needs the APIC (IPIs) and
    // the BSP's per-CPU decisions (the APs copy them).
    crate::smp::start_aps();
    serial_println!("{}", crate::cpu::render_init());

    // ── Time subsystem ─────────────────────────────────────────────
    crate::time::init();
    serial_println!("clocksource: {}", crate::time::clocksource::clocksource_name());

    // ── USB (xHCI) ─────────────────────────────────────────────────
    // Best-effort, bounded, never hangs boot — same contract as mouse and
    // AC97. Placed here rather than next to them because enumeration's
    // mandatory settling delays (a port needs ~20 ms after reset) are
    // waited out against the monotonic clock, which only exists once
    // `time::init()` above has run; and before `fs::init()` so a keyboard
    // is live by the time anything can ask to read one.
    //
    // Interrupts are still masked at this point in boot (the first `sti`
    // is in `start_first_process`), so the busy-waits inside cannot be
    // preempted — which is exactly why they are bounded by a spin count as
    // well as by the clock. See `xhci::Xhci::wait_for`.
    let mut usb_driver = crate::usb::UsbDriver::new();
    crate::hal::run_all(&mut [&mut usb_driver]);

    // ── No-input escape hatch ──────────────────────────────────────
    // If nothing on this machine can type, the shell about to start is
    // unreachable and the screen is the only diagnostic channel there is.
    // Put the boot log on it and hold, rather than booting into something
    // nobody can drive — a `cat /proc/dmesg` is worth nothing without a
    // keyboard to invoke it with.
    show_boot_log_if_no_keyboard();

    // ── VFS ────────────────────────────────────────────────────────
    crate::watchdog::test_hang_before_fs();
    crate::fs::init();
    serial_println!("VFS: initramfs @ /bin, devfs @ /dev");
    // Unattended run? From here on a panic resets instead of halting.
    crate::autorun::detect();
    // ...and the watchdog armed above stays armed only for that.
    crate::watchdog::settle();

    // ── Kernel log → USB stick ─────────────────────────────────────
    // Claims the pendrive's raw `constanos-log` partition, if it has one;
    // from here on the idle task, `sync(2)` and the panic handler copy the
    // log ring there. See `block::logpart`.
    crate::block::logpart::init();

    // ── FPU/SSE ────────────────────────────────────────────────────
    // Must run before the first `Process` is created below — every
    // constructor initializes its `fpu_state` from the template this
    // captures.
    process::fpu::init();

    // ── Processes ──────────────────────────────────────────────────
    serial_println!("\nStep 10: Creating processes");
    processes::init_all();
    processes::debug_file_descriptors();

    serial_println!("DEBUG: About to start first process");
    process::start_first_process();
}

/// Renders the USB/PCI-relevant boot log to the screen and holds it there
/// when the machine has no keyboard at all.
///
/// The gate is deliberately narrow: **no USB keyboard was enumerated and
/// the legacy 8042 does not answer**. In QEMU the 8042 always answers, so
/// this never fires there and no test flow pays for it; on a modern board
/// with the legacy controller fused out and a USB keyboard that failed to
/// come up, it is the only thing that can report why. It cannot detect the
/// in-between case — a controller present but no keyboard on it — see
/// `hal::i8042`'s module comment for why the probe stays read-only.
///
/// Holds for a fixed 30 s. There is nothing to wait *for*: interrupts are
/// still masked at this point in boot, so no keypress could cancel it even
/// if a keyboard existed. 30 s is long enough to read a screen or take a
/// photograph of it, and the boot continues afterwards — the shell may
/// still be useful over serial on a machine that has one.
/// Puts the USB/PCI part of the boot log on the screen when no USB
/// keyboard came up, and holds it there when the machine additionally has
/// no legacy 8042 — i.e. when there is provably no way to type at all.
///
/// **The dump and the hold are gated separately, and that separation was
/// learned from a bare-metal run.** Both used to require "no 8042", which
/// is wrong for the machine this exists for: its board *does* answer at
/// port 0x64 (the controller is there, nothing is plugged into it), so the
/// probe said "you have PS/2", the dump never ran, and the only thing that
/// reached the screen was the one-line red summary — the detail that says
/// *which stage* failed stayed in a serial log nobody can read there.
///
/// So: the dump costs about fifteen lines and runs whenever the USB
/// keyboard is missing, which is exactly when someone needs to read it.
/// Thanks to `FramebufferConsole::new` no longer clearing the screen, the
/// shell's output then scrolls up from underneath it rather than replacing
/// it, so those lines stay readable without stopping the boot.
///
/// The 30 s hold still needs the stricter gate: it is for the case where
/// the shell is unreachable anyway, and it must never fire in QEMU (where
/// the 8042 always answers) or every test boot would pay for it.
fn show_boot_log_if_no_keyboard() {
    let usb_keyboards = crate::usb::keyboard_count();
    let ps2 = hal::i8042::controller_present(&crate::hal::X86PortIo);
    serial_println!("input: usb keyboards={} i8042_present={}", usb_keyboards, ps2);

    if usb_keyboards > 0 {
        return;
    }

    crate::kalert!("SIN teclado USB - log de arranque (USB/PCI):");
    // Substring filters rather than log levels: see `klog::dump_to_screen`.
    crate::klog::dump_to_screen(&["usb", "xhci", "pci", "input:", "PANIC", "FAILED", "failed"]);

    if ps2 {
        // A keyboard may still be reachable through the 8042; don't stop.
        crate::kalert!("fin del log (hay 8042: se sigue arrancando)");
        return;
    }

    crate::kalert!("fin del log - sin teclado alguno, 30 s antes del shell");

    // Bounded exactly like the xHCI driver's own waits, and for the same
    // reason: with interrupts masked a jiffies-based clocksource would
    // never advance, so the spin count is what guarantees this terminates
    // rather than becoming the hang it exists to diagnose.
    let start = crate::time::ktime_get();
    let mut spins: u64 = 0;
    while crate::time::ktime_get().wrapping_sub(start) < 30_000_000_000 {
        spins += 1;
        if spins > 2_000_000_000 {
            break;
        }
        core::hint::spin_loop();
    }
}
