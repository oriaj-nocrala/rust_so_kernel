#![no_std]
#![no_main]

//! `demo`: a guided tour of this kernel's own capabilities, in one program —
//! written for a screenshot, not for testing (see the individual `_test`
//! programs and the shell's `meminfo` for that). Runs real syscalls end to
//! end (fork/exec/waitpid, the VFS across three different mounts, threads,
//! IPC, pipes) rather than just printing claims.

use userspace::{print, println, syscall};

fn color(code: &str) {
    print!("\x1b[{}m", code);
}

fn reset() {
    print!("\x1b[0m");
}

fn header(n: usize, title: &str) {
    println!();
    color("1;36");
    println!("[{}] {}", n, title);
    color("36");
    for _ in 0..(4 + title.len()) {
        print!("-");
    }
    println!();
    reset();
}

fn run(name: &str) {
    let pid = syscall::fork();
    if pid == 0 {
        syscall::with_cstr(name, |p| syscall::exec(p));
        println!("demo: exec {} failed", name);
        syscall::exit(1);
    } else if pid > 0 {
        syscall::waitpid(pid);
    }
}

/// Same getdents64 loop as the `ls` program / shell's `ls <path>` builtin —
/// duplicated locally rather than shared since none of these programs take
/// argv yet (see README).
fn list_dir(path: &str) {
    let fd = syscall::with_cstr(path, |p| syscall::open(p, 0));
    if fd < 0 {
        color("31");
        println!("  (no se pudo abrir {}: {})", path, fd);
        reset();
        return;
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
                    if name != "." && name != ".." {
                        let marker = if entry.d_type == 4 { "/" } else { "" };
                        println!("  {}{}", name, marker);
                    }
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
}

fn cat_file(path: &str) {
    let fd = syscall::with_cstr(path, |p| syscall::open(p, 0));
    if fd < 0 {
        color("31");
        println!("  (no se pudo abrir {}: {})", path, fd);
        reset();
        return;
    }
    let fd = fd as i32;
    let mut buf = [0u8; 256];
    loop {
        let n = syscall::read(fd, &mut buf);
        if n <= 0 {
            break;
        }
        syscall::write(1, &buf[..n as usize]);
    }
    syscall::close(fd);
}

fn meminfo_line(label: &str) {
    color("33");
    println!("  {}: {} KiB libres", label, syscall::meminfo_kb());
    reset();
}

#[no_mangle]
extern "C" fn _start() -> ! {
    color("1;35");
    println!("================================================");
    println!("  ConstanOS -- kernel x86_64 escrito desde cero");
    println!("  en Rust (bare-metal, #![no_std])");
    println!("================================================");
    reset();

    header(1, "uname + scheduler preemptivo (este mismo shell corre bajo el)");
    run("uname");

    header(2, "VFS propio: initramfs (/bin), devfs (/dev), ramfs (/tmp), ext2 (/mnt)");
    println!(" /bin:");
    list_dir("/bin");
    println!(" /mnt (disco real via ATA, sobrevive reboots):");
    list_dir("/mnt");
    println!(" /mnt/hello.txt:");
    color("32");
    cat_file("/mnt/hello.txt");
    reset();

    header(3, "threads reales (clone/pthread_create/join, mutex)");
    meminfo_line("antes");
    run("pthread_test");
    meminfo_line("despues");

    header(4, "IPC: sockets propios, fork + 100 round-trips");
    run("ipc_ping");

    header(5, "mmap/munmap anonimo (demand paging real)");
    run("mmap_test");

    header(6, "condvars: pthread_cond_wait/broadcast, productor/consumidor");
    run("producer_consumer");

    println!();
    color("1;32");
    println!("================================================");
    println!("  boot UEFI * buddy+slab * COW fork * threads reales");
    println!("  VFS+ext2 * IPC * senales POSIX * mlibc portado");
    println!("================================================");
    reset();

    syscall::exit(0)
}
