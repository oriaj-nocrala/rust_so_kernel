#![no_std]
#![no_main]

//! Phase 2.3 of `docs/gui/gui-plan.md`: the userspace crate's heap
//! (`userspace::heap`) and the wrappers the compositor will need —
//! `memfd_create`/`ftruncate`/shared `mmap`, `SCM_RIGHTS` through
//! `send_fds`/`recv_fds`, `epoll` and `ioctl`.
//!
//! Prints one line per check and `userlib_test: PASS` or
//! `userlib_test: FAIL (n failed)`; the exit status is the failure count.

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::Ordering;

use userspace::heap::{large_len, STATS};
use userspace::syscall::{self, EpollEvent, AF_UNIX, MAP_SHARED, PROT_READ, PROT_WRITE, SOCK_STREAM};
use userspace::println;

static mut FAILED: u32 = 0;

fn check(name: &str, ok: bool) {
    if ok {
        println!("  ok   {}", name);
    } else {
        println!("  FAIL {}", name);
        unsafe { FAILED += 1 };
    }
}

// ── heap ──────────────────────────────────────────────────────────────────

fn heap_basics() {
    let b = Box::new(0x1234_5678u64);
    let mut v: Vec<u32> = Vec::new();
    for i in 0..1000 {
        v.push(i);
    }
    let s = alloc::format!("{}-{}", "abc", 42);
    check("Box, Vec and format! work", *b == 0x1234_5678 && v.iter().sum::<u32>() == 499_500 && s == "abc-42");

    // LIFO free list: freeing a block and asking again for the same class
    // hands the same block back.
    // black_box: an allocation nothing reads can be elided altogether.
    use core::hint::black_box;
    let a = black_box(Box::new([7u8; 100]));
    let pa = &*a as *const _ as usize;
    drop(a);
    let b = black_box(Box::new([9u8; 120])); // same 128-byte class
    check("a freed small block is reused", &*b as *const _ as usize == pa);
}

fn heap_alignment() {
    let heap = &userspace::heap::HEAP;
    let mut ok = true;
    for shift in 3..=12 {
        let align = 1usize << shift;
        for size in [1usize, 24, 100, 5000, 70_000] {
            let l = Layout::from_size_align(size, align).unwrap();
            let p = unsafe { heap.alloc(l) };
            if p.is_null() || p as usize % align != 0 {
                println!("    size {} align {} -> {:p}", size, align, p);
                ok = false;
            }
            if !p.is_null() {
                unsafe { heap.dealloc(p, l) };
            }
        }
    }
    check("every alignment up to 4096 honoured", ok);
    let l = Layout::from_size_align(64, 8192).unwrap();
    check("alignment 8192 refused (null)", unsafe { heap.alloc(l) }.is_null());
}

fn heap_many_small() {
    let chunks0 = STATS.chunks.load(Ordering::Relaxed);
    // 100000 x 16 bytes = 1.6 MB: more than a chunk, so refills happen.
    let v: Vec<Box<u64>> = (0..100_000u64).map(Box::new).collect();
    let all_there = v.iter().enumerate().all(|(i, b)| **b == i as u64);
    let chunks = STATS.chunks.load(Ordering::Relaxed) - chunks0;
    check("100000 boxes, all intact", all_there);
    println!("    chunks used: {}", chunks);
    check("100000 boxes took 1-3 chunks (VMAs)", (1..=3).contains(&chunks));
}

fn heap_large() {
    let live0 = STATS.large_live.load(Ordering::Relaxed);
    let free0 = syscall::meminfo_kb();
    {
        let mut a = alloc::vec![0u8; 1 << 20];
        let mut h = alloc::vec![0u8; 3 << 20]; // 2 MiB-page path
        a[0] = 1;
        a[(1 << 20) - 1] = 2;
        for i in (0..h.len()).step_by(4096) {
            h[i] = i as u8;
        }
        check("1 MiB and 3 MiB blocks are zeroed and writable",
              a[1] == 0 && a[(1 << 20) - 1] == 2 && h[8192 + 1] == 0 && h[3 * 4096] == 0);
        check("two large blocks live", STATS.large_live.load(Ordering::Relaxed) == live0 + 2);
    }
    check("both unmapped on drop", STATS.large_live.load(Ordering::Relaxed) == live0);
    let free1 = syscall::meminfo_kb();
    println!("    MemFree before {} kB, after {} kB", free0, free1);
    check("memory given back (within 256 kB)", free1 + 256 >= free0);
    check("large_len rounds 2 MiB+ to 2 MiB pages",
          large_len(100) == 4096 && large_len(2 << 20) == 2 << 20 && large_len((2 << 20) + 1) == 4 << 20
          && large_len((2 << 20) - 100) == 2 << 20);

    // 200 x 5 MiB: a munmap that failed would leak one VMA each and run out
    // of the 64 long before the end.
    let mut ok = true;
    for i in 0..200u32 {
        let mut v: Vec<u32> = Vec::with_capacity(5 << 18);
        v.push(i);
        if v[0] != i {
            ok = false;
        }
    }
    check("200 x 5 MiB alloc/free (exact munmap every time)",
          ok && STATS.large_live.load(Ordering::Relaxed) == live0);
}

fn heap_realloc() {
    let mut v: Vec<u32> = Vec::new();
    for i in 0..1_000_000u32 {
        v.push(i.wrapping_mul(2_654_435_761));
    }
    let ok = v.iter().enumerate().all(|(i, x)| *x == (i as u32).wrapping_mul(2_654_435_761));
    check("Vec grown to 4 MB through every path keeps its contents", ok);
    let mut s = String::new();
    for _ in 0..10_000 {
        s.push_str("xy");
    }
    check("String of 20000 bytes", s.len() == 20_000 && s.ends_with("xy"));
}

// ── shared memory, fd passing, epoll, ioctl ───────────────────────────────

fn memfd_basics() {
    let fd = syscall::memfd_create(b"t\0", syscall::MFD_CLOEXEC);
    check("memfd_create", fd >= 0);
    if fd < 0 {
        return;
    }
    let fd = fd as i32;
    check("ftruncate 64 KiB", syscall::ftruncate(fd, 65536) == 0);
    let a = syscall::mmap(0, 65536, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    let b = syscall::mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 8192);
    check("two MAP_SHARED mappings", a > 0 && b > 0);
    if a > 0 && b > 0 {
        unsafe {
            *((a as *mut u32).add(8192 / 4)) = 0xC0FF_EE00;
            check("a write through one is seen through the other", *(b as *const u32) == 0xC0FF_EE00);
        }
        check("shrinking a mapped memfd is EBUSY", syscall::ftruncate(fd, 4096) == -16);
        check("munmap both", syscall::munmap(a as u64, 65536) == 0 && syscall::munmap(b as u64, 4096) == 0);
    }
    syscall::close(fd);
}

fn scm_rights() {
    let mut sv = [0i32; 2];
    if syscall::socketpair(AF_UNIX as i32, SOCK_STREAM, 0, &mut sv) < 0 {
        check("socketpair", false);
        return;
    }
    let mfd = syscall::memfd_create(b"pass\0", 0) as i32;
    syscall::ftruncate(mfd, 4096);
    let base = syscall::mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);

    let pid = syscall::fork();
    if pid == 0 {
        // Child: receive the memfd, write through its own mapping, ack.
        syscall::close(sv[0]);
        let mut buf = [0u8; 8];
        let mut fds = [-1i32; 2];
        let code = match syscall::recv_fds(sv[1], &mut buf, &mut fds, 0) {
            Ok(r) if r.nfds == 1 && r.len == 1 && fds[0] >= 0 && fds[0] != mfd => {
                let m = syscall::mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fds[0], 0);
                if m > 0 {
                    let msg = b"hello from the child";
                    unsafe { core::ptr::copy_nonoverlapping(msg.as_ptr(), m as *mut u8, msg.len()) };
                    0
                } else {
                    3
                }
            }
            Ok(_) => 2,
            Err(_) => 1,
        };
        syscall::send(sv[1], &[code]);
        syscall::exit(code as i32);
    }
    syscall::close(sv[1]);
    let sent = syscall::send_fds(sv[0], b"m", &[mfd], 0);
    check("send_fds with one memfd", sent == 1);
    let mut ack = [0xFFu8; 1];
    syscall::recv(sv[0], &mut ack);
    let (_, status) = syscall::waitpid_status(pid);
    println!("    child ack {} status {:#x}", ack[0], status);
    check("child received exactly one new fd and mapped it", ack[0] == 0);
    let got = unsafe { core::slice::from_raw_parts(base as *const u8, 20) };
    check("its write is visible in our mapping", got == b"hello from the child");

    // Two fds into room for one: the second is closed, MSG_CTRUNC is set.
    let mut sv2 = [0i32; 2];
    syscall::socketpair(AF_UNIX as i32, SOCK_STREAM, 0, &mut sv2);
    syscall::send_fds(sv2[0], b"z", &[mfd, mfd], 0);
    let mut buf = [0u8; 4];
    let mut one = [-1i32; 1];
    match syscall::recv_fds(sv2[1], &mut buf, &mut one, 0) {
        Ok(r) => {
            check("2 fds into room for 1: one delivered, MSG_CTRUNC",
                  r.nfds == 1 && r.flags & syscall::MSG_CTRUNC != 0 && one[0] >= 0);
            syscall::close(one[0]);
        }
        Err(e) => {
            println!("    recv_fds -> {}", e);
            check("2 fds into room for 1", false);
        }
    }
    check("more than MAX_PASSED_FDS is EINVAL",
          syscall::send_fds(sv2[0], b"z", &[mfd; syscall::MAX_PASSED_FDS + 1], 0) == -22);
    for f in [sv[0], sv2[0], sv2[1], mfd] {
        syscall::close(f);
    }
    syscall::munmap(base as u64, 4096);
}

fn epoll_basics() {
    let mut sv = [0i32; 2];
    syscall::socketpair(AF_UNIX as i32, SOCK_STREAM, 0, &mut sv);
    let ep = syscall::epoll_create();
    check("epoll_create", ep >= 0);
    let ep = ep as i32;
    const TAG: u64 = 0xDEAD_BEEF_CAFE;
    check("epoll_ctl ADD", syscall::epoll_ctl(ep, syscall::EPOLL_CTL_ADD, sv[1], syscall::EPOLLIN, TAG) == 0);
    let mut evs = [EpollEvent::default(); 4];
    check("nothing ready: epoll_wait(0) = 0", syscall::epoll_wait(ep, &mut evs, 0) == 0);
    syscall::send(sv[0], b"!");
    let n = syscall::epoll_wait(ep, &mut evs, 1000);
    let (events, data) = (evs[0].events, evs[0].data);
    check("readable: one event with EPOLLIN and our tag",
          n == 1 && events & syscall::EPOLLIN != 0 && data == TAG);
    for f in [ep, sv[0], sv[1]] {
        syscall::close(f);
    }
}

fn ioctl_winsz() {
    const TIOCGWINSZ: u64 = 0x5413;
    let mut ws = [0u16; 4];
    let r = syscall::ioctl(1, TIOCGWINSZ, ws.as_mut_ptr() as u64);
    println!("    TIOCGWINSZ on fd 1: {} rows x {} cols", ws[0], ws[1]);
    check("ioctl(TIOCGWINSZ)", r == 0 && ws[0] > 0 && ws[1] > 0);
}

userspace::entry!(main);

fn main(_args: userspace::args::Args) -> i32 {
    println!("userlib_test: heap");
    heap_basics();
    heap_alignment();
    heap_many_small();
    heap_large();
    heap_realloc();
    println!("userlib_test: wrappers");
    memfd_basics();
    scm_rights();
    epoll_basics();
    ioctl_winsz();

    let failed = unsafe { FAILED };
    if failed == 0 {
        println!("userlib_test: PASS");
    } else {
        println!("userlib_test: FAIL ({} failed)", failed);
    }
    syscall::exit(failed as i32)
}
