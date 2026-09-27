#![no_std]

extern crate alloc;

pub mod syscall;
pub mod fmt;
pub mod heap;
pub mod args;
pub mod gfx;
pub mod text;
pub mod fs;
pub mod img;
pub mod launch;

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit(101)
}
