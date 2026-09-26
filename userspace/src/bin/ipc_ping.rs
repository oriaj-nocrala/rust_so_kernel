#![no_std]
#![no_main]

//! AF_UNIX stream round-trip: `socket`/`bind`/`listen`/`accept` on one side,
//! `connect`/`send`/`recv` on the other, 100 times.
//!
//! Uses the **abstract namespace** (`sun_path[0] == '\0'`), so the address
//! needs no filesystem node and the test leaves nothing behind. The server
//! `listen()`s before it forks, which is what lets the client connect
//! immediately instead of retry-looping the way this test had to when
//! `accept()` couldn't block.

use userspace::syscall::{SockAddrUn, AF_UNIX, SOCK_STREAM};
use userspace::{println, syscall};

const NAME: &[u8] = b"ipc-ping";
const ROUNDS: u32 = 100;

fn client() -> ! {
    let fd = syscall::socket(AF_UNIX as i32, SOCK_STREAM, 0);
    if fd < 0 {
        println!("ipc_ping: client socket failed ({})", fd);
        syscall::exit(1);
    }
    let fd = fd as i32;

    let (addr, len) = SockAddrUn::abstract_name(NAME);
    let r = syscall::connect(fd, &addr, len);
    if r < 0 {
        println!("ipc_ping: connect failed ({})", r);
        syscall::exit(1);
    }

    let mut ok = 0u32;
    let mut reply = [0u8; 16];
    for i in 0..ROUNDS {
        let payload = [b'p', b'i', b'n', b'g', i as u8];
        let s = syscall::send(fd, &payload);
        if s < 0 {
            println!("ipc_ping: client send failed at round {} ({})", i, s);
            break;
        }
        let r = syscall::recv(fd, &mut reply);
        if r < 0 {
            println!("ipc_ping: client recv failed at round {} ({})", i, r);
            break;
        }
        if r as usize == payload.len() && reply[..payload.len()] == payload {
            ok += 1;
        }
    }

    println!("ipc_ping: client done, {}/{} round-trips ok", ok, ROUNDS);
    syscall::exit(0);
}

userspace::entry!(main);

fn main(_args: userspace::args::Args) -> i32 {
    let fd = syscall::socket(AF_UNIX as i32, SOCK_STREAM, 0);
    if fd < 0 {
        println!("ipc_ping: server socket failed ({})", fd);
        syscall::exit(1);
    }
    let fd = fd as i32;

    let (addr, len) = SockAddrUn::abstract_name(NAME);
    let b = syscall::bind(fd, &addr, len);
    if b < 0 {
        println!("ipc_ping: bind failed ({})", b);
        syscall::exit(1);
    }
    let l = syscall::listen(fd, 4);
    if l < 0 {
        println!("ipc_ping: listen failed ({})", l);
        syscall::exit(1);
    }

    let pid = syscall::fork();
    if pid == 0 {
        client();
    } else if pid < 0 {
        println!("ipc_ping: fork failed ({})", pid);
        syscall::exit(1);
    }

    let peer = syscall::accept(fd);
    if peer < 0 {
        println!("ipc_ping: accept failed ({})", peer);
        syscall::exit(1);
    }
    let peer = peer as i32;

    let mut ok = 0u32;
    let mut buf = [0u8; 16];
    for i in 0..ROUNDS {
        let r = syscall::recv(peer, &mut buf);
        if r < 0 {
            println!("ipc_ping: server recv failed at round {} ({})", i, r);
            break;
        }
        if r == 0 {
            println!("ipc_ping: client hung up at round {}", i);
            break;
        }
        let s = syscall::send(peer, &buf[..r as usize]);
        if s < 0 {
            println!("ipc_ping: server send failed at round {} ({})", i, s);
            break;
        }
        ok += 1;
    }

    syscall::waitpid(pid);
    println!("ipc_ping: server done, {}/{} round-trips echoed", ok, ROUNDS);
    syscall::exit(0);
}
