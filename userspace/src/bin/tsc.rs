#![no_std]
#![no_main]

use userspace::{println, syscall};

#[no_mangle]
extern "C" fn _start() -> ! {
    let (sec0, nsec0) = syscall::clock_gettime();
    let start_ms = syscall::uptime_ms();

    // Busy-loop a bit, then sleep, to show both timing paths working.
    let mut x: u64 = 0;
    for i in 0..5_000_000u64 {
        x = x.wrapping_add(i);
    }
    syscall::sleep_ms(500);

    let (sec1, nsec1) = syscall::clock_gettime();
    let end_ms = syscall::uptime_ms();

    let elapsed_ms = end_ms - start_ms;
    let mut delta_ns = (sec1 - sec0) * 1_000_000_000 + (nsec1 - nsec0);
    if delta_ns < 0 {
        delta_ns = 0;
    }

    println!("clock_gettime: start=({}, {}) end=({}, {})", sec0, nsec0, sec1, nsec1);
    println!("elapsed (clock_gettime): {} ns", delta_ns);
    println!("elapsed (uptime_ms):     {} ms", elapsed_ms);
    println!("(busy-loop checksum: {})", x);

    syscall::exit(0)
}
