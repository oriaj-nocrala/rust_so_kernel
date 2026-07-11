#![no_std]
#![no_main]

use userspace::{println, syscall};

#[no_mangle]
extern "C" fn _start() -> ! {
    let secs = syscall::uptime_sec();
    let mins = secs / 60;
    let rem_secs = secs % 60;
    println!("up {} min, {} sec ({} ms)", mins, rem_secs, syscall::uptime_ms());
    syscall::exit(0)
}
