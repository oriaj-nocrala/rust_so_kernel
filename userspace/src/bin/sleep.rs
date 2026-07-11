#![no_std]
#![no_main]

use userspace::{println, syscall};

const SLEEP_MS: u64 = 2000;

#[no_mangle]
extern "C" fn _start() -> ! {
    println!("sleeping for {} ms...", SLEEP_MS);
    syscall::sleep_ms(SLEEP_MS);
    println!("done");
    syscall::exit(0)
}
