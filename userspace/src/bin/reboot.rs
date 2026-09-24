#![no_std]
#![no_main]

// `reboot` — restarts the machine through `reboot(2)`. The kernel flushes
// its log to the USB stick before resetting (see kernel/src/reboot.rs);
// every filesystem write is already on disk by the time it returns.

use userspace::{println, syscall};

#[no_mangle]
extern "C" fn _start() -> ! {
    println!("Reiniciando...");
    let r = syscall::reboot();
    println!("reboot: failed ({})", r);
    syscall::exit(1)
}
