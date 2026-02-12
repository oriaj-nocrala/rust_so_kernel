#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

mod allocator;
mod framebuffer;
mod init;
mod interrupts;
mod keyboard;
mod keyboard_buffer;
mod memory;
mod panic;
mod process;
mod pit;
mod repl;
mod serial;

use bootloader_api::{BootInfo, BootloaderConfig, config::Mapping, entry_point};

pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    init::boot(boot_info)
}