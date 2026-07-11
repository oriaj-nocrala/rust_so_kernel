#![no_std]
#![no_main]

use userspace::{println, syscall};
use userspace::syscall::{PROT_READ, PROT_WRITE};

const PAGE_SIZE: u64 = 4096;
const NUM_PAGES: u64 = 3;
const LEN: u64 = PAGE_SIZE * NUM_PAGES;

#[no_mangle]
extern "C" fn _start() -> ! {
    let addr = syscall::mmap_anon(0, LEN, PROT_READ | PROT_WRITE);
    if addr < 0 {
        println!("mmap_test: mmap failed ({})", addr);
        println!("FAIL");
        syscall::exit(1);
    }
    let addr = addr as u64;

    // Offsets spread across all three pages, including right near the end
    // of the mapping, to prove every page is actually mapped and writable.
    let offsets: [u64; 7] = [
        0,
        1,
        PAGE_SIZE - 1,
        PAGE_SIZE,
        2 * PAGE_SIZE,
        2 * PAGE_SIZE + 100,
        LEN - 1,
    ];

    let mut ok = true;
    unsafe {
        for (i, &off) in offsets.iter().enumerate() {
            let ptr = (addr as *mut u8).add(off as usize);
            let pattern = (i as u8).wrapping_mul(37).wrapping_add(0xA5);
            ptr.write(pattern);
        }
        for (i, &off) in offsets.iter().enumerate() {
            let ptr = (addr as *const u8).add(off as usize);
            let expected = (i as u8).wrapping_mul(37).wrapping_add(0xA5);
            let got = ptr.read();
            if got != expected {
                println!(
                    "mmap_test: mismatch at offset {} (expected {}, got {})",
                    off, expected, got
                );
                ok = false;
            }
        }
    }

    let m = syscall::munmap(addr, LEN);
    if m < 0 {
        println!("mmap_test: munmap failed ({})", m);
        ok = false;
    }

    if ok {
        println!("PASS");
        syscall::exit(0);
    } else {
        println!("FAIL");
        syscall::exit(1);
    }
}
