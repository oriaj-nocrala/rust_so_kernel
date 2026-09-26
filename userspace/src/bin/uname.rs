#![no_std]
#![no_main]

use userspace::{println, syscall};

userspace::entry!(main);

fn main(_args: userspace::args::Args) -> i32 {
    println!("ConstanOS 0.1.0 x86_64");
    syscall::exit(0)
}
