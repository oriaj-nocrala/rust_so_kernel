#![no_std]
#![no_main]

//! `poll()` on an AF_UNIX stream socket, over a **pathname** address — so
//! this also exercises the filesystem half of `bind()`: the socket node in
//! `/tmp`, which `connect()` resolves and `unlink()` removes.
//!
//! The server blocks in `poll()` with nothing queued; the client sends after
//! a deliberate delay, and the wakeup has to come from the socket layer
//! itself (`poll_wakeup_for_socket`), not from the poll timeout.

use userspace::syscall::{SockAddrUn, AF_UNIX, SOCK_STREAM};
use userspace::syscall::{PollFd, POLLIN};
use userspace::{println, syscall};

const PATH: &[u8] = b"/tmp/poll_test.sock";

fn client() -> ! {
    let fd = syscall::socket(AF_UNIX as i32, SOCK_STREAM, 0);
    if fd < 0 {
        syscall::exit(1);
    }
    let fd = fd as i32;

    let (addr, len) = SockAddrUn::path(PATH);
    if syscall::connect(fd, &addr, len) < 0 {
        syscall::exit(1);
    }

    // Long enough that the server is genuinely parked in poll() by now.
    syscall::sleep_ms(200);
    syscall::send(fd, b"hello-poll");
    syscall::exit(0);
}

#[no_mangle]
extern "C" fn _start() -> ! {
    // A leftover node from an earlier run would make bind() fail with
    // EADDRINUSE — which is correct behavior, so clean up first.
    syscall::with_cstr("/tmp/poll_test.sock", |p| syscall::unlink(p));

    let fd = syscall::socket(AF_UNIX as i32, SOCK_STREAM, 0);
    if fd < 0 {
        println!("poll_test: server socket failed ({})", fd);
        println!("FAIL");
        syscall::exit(1);
    }
    let fd = fd as i32;

    let (addr, len) = SockAddrUn::path(PATH);
    let b = syscall::bind(fd, &addr, len);
    if b < 0 {
        println!("poll_test: bind failed ({})", b);
        println!("FAIL");
        syscall::exit(1);
    }
    if syscall::listen(fd, 4) < 0 {
        println!("poll_test: listen failed");
        println!("FAIL");
        syscall::exit(1);
    }

    let pid = syscall::fork();
    if pid == 0 {
        client();
    } else if pid < 0 {
        println!("poll_test: fork failed ({})", pid);
        println!("FAIL");
        syscall::exit(1);
    }

    let peer = syscall::accept(fd);
    if peer < 0 {
        println!("poll_test: accept failed ({})", peer);
        println!("FAIL");
        syscall::exit(1);
    }
    let peer = peer as i32;

    let mut fds = [PollFd { fd: peer, events: POLLIN, revents: 0 }];
    let r = syscall::poll(&mut fds, 2000);

    let mut ok = true;
    if r < 0 {
        println!("poll_test: poll failed ({})", r);
        ok = false;
    } else if fds[0].revents & POLLIN == 0 {
        println!("poll_test: poll returned without POLLIN (revents={})", fds[0].revents);
        ok = false;
    } else {
        let mut buf = [0u8; 32];
        let rv = syscall::recv(peer, &mut buf);
        if rv < 0 {
            println!("poll_test: recv failed ({})", rv);
            ok = false;
        } else if &buf[..rv as usize] != b"hello-poll" {
            println!("poll_test: unexpected message contents");
            ok = false;
        }
    }

    syscall::waitpid(pid);
    syscall::with_cstr("/tmp/poll_test.sock", |p| syscall::unlink(p));

    if ok {
        println!("PASS");
        syscall::exit(0);
    } else {
        println!("FAIL");
        syscall::exit(1);
    }
}
