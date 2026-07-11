#![no_std]
#![no_main]

use userspace::{println, syscall};

#[no_mangle]
extern "C" fn _start() -> ! {
    println!("ConstanOS 0.1.0 x86_64");
    syscall::exit(0)
}
