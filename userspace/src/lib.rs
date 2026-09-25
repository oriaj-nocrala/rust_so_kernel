#![no_std]

extern crate alloc;

pub mod syscall;
pub mod fmt;
pub mod heap;
pub mod args;

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit(101)
}
