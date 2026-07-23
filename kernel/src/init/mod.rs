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

    // ── Memory subsystem ───────────────────────────────────────────
    let phys_mem_offset = VirtAddr::new(
        boot_info.physical_memory_offset.into_option().unwrap()
    );

    memory::init_core(phys_mem_offset, &boot_info.memory_regions);

    // Allocate and zero-fill the shared zero frame (used by the zero-page trick).
    unsafe { crate::memory::cow::init_zero_frame(); }

    memory::test_allocators();

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
    serial_println!("TSC: {} MHz", crate::cpu::tsc::freq_hz() / 1_000_000);

    // ── Time subsystem ─────────────────────────────────────────────
    crate::time::init();
    serial_println!("clocksource: {}", crate::time::clocksource::clocksource_name());

    // ── VFS ────────────────────────────────────────────────────────
    crate::fs::init();
    serial_println!("VFS: initramfs @ /bin, devfs @ /dev");

    // ── TSS + GDT ──────────────────────────────────────────────────
    serial_println!("Step 9: Initializing TSS and GDT");
    process::tss::init();
    process::tss::init_syscall_msrs();

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