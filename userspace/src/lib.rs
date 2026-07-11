#![no_std]

pub mod syscall;
pub mod fmt;

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit(101)
}
