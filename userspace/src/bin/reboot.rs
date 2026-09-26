#![no_std]
#![no_main]

// `reboot` — restarts the machine through `reboot(2)`. The kernel flushes
// its log to the USB stick before resetting (see kernel/src/reboot.rs);
// every filesystem write is already on disk by the time it returns.

use userspace::{println, syscall};

userspace::entry!(main);

fn main(_args: userspace::args::Args) -> i32 {
    println!("Reiniciando...");
    let r = syscall::reboot();
    println!("reboot: failed ({})", r);
    syscall::exit(1)
}
