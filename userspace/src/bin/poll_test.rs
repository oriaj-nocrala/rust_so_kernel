#![no_std]
#![no_main]

use userspace::{println, syscall};
use userspace::syscall::{IpcMsg, PollFd, POLLIN};

const CHANNEL: &str = "/ipc/poll";

fn client() -> ! {
    let fd = syscall::socket();
    if fd < 0 {
        syscall::exit(1);
    }
    let fd = fd as i32;

    loop {
        let r = syscall::with_cstr(CHANNEL, |p| syscall::connect(fd, p));
        if r >= 0 {
            break;
        }
        syscall::sleep_ms(10);
    }

    // Give the server a moment to be sitting in poll() before we send.
    syscall::sleep_ms(200);

    let msg = IpcMsg::new(1, b"hello-poll");
    syscall::sendmsg(fd, &msg);
    syscall::exit(0);
}

#[no_mangle]
extern "C" fn _start() -> ! {
    let fd = syscall::socket();
    if fd < 0 {
        println!("poll_test: server socket failed ({})", fd);
        println!("FAIL");
        syscall::exit(1);
    }
    let fd = fd as i32;

    let b = syscall::with_cstr(CHANNEL, |p| syscall::bind(fd, p));
    if b < 0 {
        println!("poll_test: bind failed ({})", b);
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
        let mut msg = IpcMsg::new(0, &[]);
        let rv = syscall::recvmsg(peer, &mut msg);
        if rv < 0 {
            println!("poll_test: recvmsg failed ({})", rv);
            ok = false;
        } else if msg.tag != 1 || &msg.data[..msg.len as usize] != b"hello-poll" {
            println!("poll_test: unexpected message contents");
            ok = false;
        }
    }

    syscall::waitpid(pid);

    if ok {
        println!("PASS");
        syscall::exit(0);
    } else {
        println!("FAIL");
        syscall::exit(1);
    }
}
