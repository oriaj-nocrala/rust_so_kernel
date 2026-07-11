#![no_std]
#![no_main]

use userspace::{eprintln, println, syscall};

fn type_marker(d_type: u8) -> &'static str {
    match d_type {
        4 => "/", // DT_DIR
        2 => "@", // DT_CHR
        8 => "",  // DT_REG
        _ => "?",
    }
}

#[no_mangle]
extern "C" fn _start() -> ! {
    let fd = syscall::with_cstr("/", |p| syscall::open(p, 0));
    if fd < 0 {
        eprintln!("ls: cannot open /: {}", fd);
        syscall::exit(1);
    }
    let fd = fd as i32;

    let mut buf = [0u8; 512];
    loop {
        let n = syscall::getdents64(fd, &mut buf);
        if n <= 0 {
            break;
        }
        let n = n as usize;
        let mut off = 0usize;
        while off < n {
            match syscall::parse_dirent(&buf[off..n]) {
                Some(entry) => {
                    let name = core::str::from_utf8(entry.name).unwrap_or("?");
                    println!("{}{}", name, type_marker(entry.d_type));
                    if entry.record_len == 0 {
                        break;
                    }
                    off += entry.record_len;
                }
                None => break,
            }
        }
    }

    syscall::close(fd);
    syscall::exit(0)
}
