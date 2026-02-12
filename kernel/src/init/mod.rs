// kernel/src/init/mod.rs
//
// Boot orchestration — calls sub-modules in the exact order
// the original kernel_main did.

pub mod devices;
pub mod memory;
pub mod processes;

use bootloader_api::BootInfo;
use x86_64::VirtAddr;

use crate::{
    framebuffer::{self, Color, Framebuffer, init_global_framebuffer},
    process,
    repl::Repl,
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
    memory::test_allocators();

    // ── Boot screen ────────────────────────────────────────────────
    devices::draw_boot_screen();

    // ── Hardware interrupts ────────────────────────────────────────
    devices::init_hardware_interrupts();

    // ── Repl (initial prompt, will be replaced by shell process) ───
    let mut repl = Repl::new(10, 50);
    repl.show_prompt();

    // ── TSS + GDT ──────────────────────────────────────────────────
    serial_println!("Step 9: Initializing TSS and GDT");
    process::tss::init();

    // ── Processes ──────────────────────────────────────────────────
    serial_println!("\nStep 10: Creating processes");
    processes::init_all();
    processes::debug_file_descriptors();

    serial_println!("DEBUG: About to start first process");
    process::start_first_process();
}