#![no_std]
#![no_main]

use userspace::{println, syscall};
use userspace::syscall::IpcMsg;

const CHANNEL: &str = "/ipc/ping";
const ROUNDS: u32 = 100;

fn client() -> ! {
    let fd = syscall::socket();
    if fd < 0 {
        println!("ipc_ping: client socket failed ({})", fd);
        syscall::exit(1);
    }
    let fd = fd as i32;

    // Retry connect until the server has accepted (no blocking-accept
    // guarantee before connect, so poll with a short sleep).
    loop {
        let r = syscall::with_cstr(CHANNEL, |p| syscall::connect(fd, p));
        if r >= 0 {
            break;
        }
        syscall::sleep_ms(10);
    }

    let mut ok = 0u32;
    for i in 0..ROUNDS {
        let payload = [b'p', b'i', b'n', b'g'];
        let msg = IpcMsg::new(i, &payload);
        let s = syscall::sendmsg(fd, &msg);
        if s < 0 {
            println!("ipc_ping: client send failed at round {} ({})", i, s);
            break;
        }
        let mut reply = IpcMsg::new(0, &[]);
        let r = syscall::recvmsg(fd, &mut reply);
        if r < 0 {
            println!("ipc_ping: client recv failed at round {} ({})", i, r);
            break;
        }
        if reply.tag == i && reply.len == msg.len && reply.data[..reply.len as usize] == msg.data[..msg.len as usize] {
            ok += 1;
        }
    }

    println!("ipc_ping: client done, {}/{} round-trips ok", ok, ROUNDS);
    syscall::exit(0);
}

#[no_mangle]
extern "C" fn _start() -> ! {
    let fd = syscall::socket();
    if fd < 0 {
        println!("ipc_ping: server socket failed ({})", fd);
        syscall::exit(1);
    }
    let fd = fd as i32;

    let b = syscall::with_cstr(CHANNEL, |p| syscall::bind(fd, p));
    if b < 0 {
        println!("ipc_ping: bind failed ({})", b);
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
    for i in 0..ROUNDS {
        let mut msg = IpcMsg::new(0, &[]);
        let r = syscall::recvmsg(peer, &mut msg);
        if r < 0 {
            println!("ipc_ping: server recv failed at round {} ({})", i, r);
            break;
        }
        let s = syscall::sendmsg(peer, &msg);
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
