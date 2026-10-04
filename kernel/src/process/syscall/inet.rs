// kernel/src/process/syscall/inet.rs
//
// The AF_INET (UDP) half of the socket syscalls. `ipc.rs` hands a call here
// as soon as the descriptor turns out to be an inet socket (`network::is_inet`
// on its `socket_id()`); the AF_UNIX path is untouched.
//
// Same shape as `ipc.rs` (see its header): do the work under the stack's lock
// (`network::*` does, and applies the wakeups afterwards), and if the answer
// was `Again` either return `-EAGAIN` (`O_NONBLOCK`, `MSG_DONTWAIT`) or park
// with `unix::block_on`, which never returns and restarts the syscall.
// Nothing with a `Drop` may be live across `block_on`: everything here is
// plain integers and `Copy` endpoints.

use alloc::boxed::Box;

use net::smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address};
use net::NetError;

use crate::ipc::unix;
use crate::network::{self, InetSocketHandle};

use super::{errno, validate_user_buffer, SyscallResult};

pub(super) const AF_INET: i32 = 2;
const AF_UNSPEC: u16 = 0;
const SOCK_DGRAM: i32 = 2;
const IPPROTO_UDP: i32 = 17;
const SOCK_NONBLOCK: i32 = 0o4000;
const SOCK_CLOEXEC: i32 = 0o2000000;

const MSG_PEEK: u32 = 2;
const MSG_TRUNC: u32 = 0x20;
const MSG_DONTWAIT: u32 = 0x40;

const SOL_SOCKET: i32 = 1;
const SO_REUSEADDR: i32 = 2;
const SO_TYPE: i32 = 3;
const SO_ERROR: i32 = 4;
const SO_BROADCAST: i32 = 6;
const SO_SNDBUF: i32 = 7;
const SO_RCVBUF: i32 = 8;
const SO_KEEPALIVE: i32 = 9;
const SO_LINGER: i32 = 13;
const SO_REUSEPORT: i32 = 15;

const EAFNOSUPPORT: i64 = -97;
const EPROTONOSUPPORT: i64 = -93;
const ESOCKTNOSUPPORT: i64 = -94;
const ENOPROTOOPT: i64 = -92;
const EOPNOTSUPP: i64 = -95;
const EMFILE: i64 = -24;

/// `sizeof(struct sockaddr_in)`.
const SOCKADDR_IN_LEN: usize = 16;

fn errno_of(e: NetError) -> i64 {
    -(e.errno() as i64)
}

/// `struct sockaddr_in` from user memory: `(address, port)`.
fn read_sockaddr_in(ptr: u64, len: u64) -> Result<(Ipv4Address, u16), i64> {
    if (len as usize) < SOCKADDR_IN_LEN {
        return Err(errno::EINVAL);
    }
    validate_user_buffer(ptr, SOCKADDR_IN_LEN)?;
    // SAFETY: validated above.
    let b = unsafe { core::slice::from_raw_parts(ptr as *const u8, SOCKADDR_IN_LEN) };
    if u16::from_le_bytes([b[0], b[1]]) != AF_INET as u16 {
        return Err(EAFNOSUPPORT);
    }
    Ok((Ipv4Address::new(b[4], b[5], b[6], b[7]), u16::from_be_bytes([b[2], b[3]])))
}

/// Writes a `sockaddr_in` through the `sockaddr*` / `socklen_t*` pair the
/// way `getsockname`/`recvfrom` do: at most what the caller's buffer holds,
/// with the true length reported. Null pointers mean "don't care".
fn write_sockaddr_in(addr: Ipv4Address, port: u16, ptr: u64, len_ptr: u64) -> Result<(), i64> {
    if ptr == 0 || len_ptr == 0 {
        return Ok(());
    }
    validate_user_buffer(len_ptr, 4)?;
    // SAFETY: validated above.
    let cap = (unsafe { *(len_ptr as *const u32) } as usize).min(SOCKADDR_IN_LEN);
    if cap > 0 {
        validate_user_buffer(ptr, cap)?;
    }
    let mut buf = [0u8; SOCKADDR_IN_LEN];
    buf[..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
    buf[2..4].copy_from_slice(&port.to_be_bytes());
    buf[4..8].copy_from_slice(&addr.octets());
    // SAFETY: both ranges validated; `cap` <= the buffer.
    unsafe {
        core::ptr::copy_nonoverlapping(buf.as_ptr(), ptr as *mut u8, cap);
        *(len_ptr as *mut u32) = SOCKADDR_IN_LEN as u32;
    }
    Ok(())
}

fn endpoint(addr: Ipv4Address, port: u16) -> IpEndpoint {
    IpEndpoint::new(IpAddress::Ipv4(addr), port)
}

pub(super) fn socket(ty: i32, protocol: i32) -> SyscallResult {
    let kind = ty & !(SOCK_NONBLOCK | SOCK_CLOEXEC);
    match kind {
        SOCK_DGRAM => {}
        1 => return EPROTONOSUPPORT, // TCP: not yet
        _ => return ESOCKTNOSUPPORT,
    }
    if protocol != 0 && protocol != IPPROTO_UDP {
        return EPROTONOSUPPORT;
    }
    let id = match network::udp_open() {
        Ok(id) => id,
        Err(_) => return EAFNOSUPPORT, // no NIC: as if the family were not built
    };
    let handle = InetSocketHandle::new(id);
    handle.set_nonblocking(ty & SOCK_NONBLOCK != 0);
    let files = super::ipc::current_files();
    let allocated = files.lock().allocate(Box::new(handle));
    match allocated {
        Ok(fd) => {
            if ty & SOCK_CLOEXEC != 0 {
                super::fs::set_cloexec_current(fd);
            }
            fd as i64
        }
        Err(_) => EMFILE, // the dropped handle released the socket
    }
}

pub(super) fn bind(id: usize, addr_ptr: u64, addrlen: u64) -> SyscallResult {
    let (addr, port) = match read_sockaddr_in(addr_ptr, addrlen) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let addr = if addr.is_unspecified() { None } else { Some(addr) };
    match network::udp_bind(id, addr, port) {
        Ok(_) => 0,
        Err(e) => errno_of(e),
    }
}

pub(super) fn connect(id: usize, addr_ptr: u64, addrlen: u64) -> SyscallResult {
    // AF_UNSPEC dissolves the default destination; not tracked, accepted.
    if addrlen >= 2 && validate_user_buffer(addr_ptr, 2).is_ok() {
        // SAFETY: validated above.
        let fam = unsafe { core::ptr::read_unaligned(addr_ptr as *const u16) };
        if fam == AF_UNSPEC {
            return 0;
        }
    }
    let (addr, port) = match read_sockaddr_in(addr_ptr, addrlen) {
        Ok(v) => v,
        Err(e) => return e,
    };
    match network::udp_connect(id, endpoint(addr, port)) {
        Ok(()) => 0,
        Err(e) => errno_of(e),
    }
}

pub(super) fn sendto(
    id: usize,
    fd: i32,
    buf: u64,
    len: usize,
    flags: u32,
    dest_ptr: u64,
    dest_len: u64,
) -> SyscallResult {
    if len > 0 {
        if let Err(e) = validate_user_buffer(buf, len) {
            return e;
        }
    }
    let dest = if dest_ptr != 0 && dest_len >= 2 {
        match read_sockaddr_in(dest_ptr, dest_len) {
            Ok((a, p)) => Some(endpoint(a, p)),
            Err(e) => return e,
        }
    } else {
        None
    };
    // SAFETY: validated above.
    let data: &[u8] = if len == 0 { &[] } else { unsafe { core::slice::from_raw_parts(buf as *const u8, len) } };

    let epoch = unix::wake_epoch();
    match network::udp_send(id, data, dest) {
        Ok(n) => n as i64,
        Err(NetError::Again) => {
            if flags & MSG_DONTWAIT != 0 || unix::fd_is_nonblocking(fd) {
                return errno::EAGAIN;
            }
            unix::block_on(id, epoch)
        }
        Err(e) => errno_of(e),
    }
}

pub(super) fn recvfrom(
    id: usize,
    fd: i32,
    buf: u64,
    len: usize,
    flags: u32,
    src_ptr: u64,
    src_len_ptr: u64,
) -> SyscallResult {
    if len > 0 {
        if let Err(e) = validate_user_buffer(buf, len) {
            return e;
        }
    }
    // SAFETY: validated above.
    let out: &mut [u8] = if len == 0 { &mut [] } else { unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len) } };

    let epoch = unix::wake_epoch();
    let (n, full, from) = match network::udp_recv(id, out, flags & MSG_PEEK != 0) {
        Ok(v) => v,
        Err(NetError::Again) => {
            if flags & MSG_DONTWAIT != 0 || unix::fd_is_nonblocking(fd) {
                return errno::EAGAIN;
            }
            unix::block_on(id, epoch)
        }
        Err(e) => return errno_of(e),
    };
    if let Some(a) = network::ipv4_of(&from) {
        if let Err(e) = write_sockaddr_in(a, from.port, src_ptr, src_len_ptr) {
            return e;
        }
    }
    if flags & MSG_TRUNC != 0 { full as i64 } else { n as i64 }
}

pub(super) fn name(id: usize, addr_ptr: u64, len_ptr: u64, peer: bool) -> SyscallResult {
    let (addr, port) = if peer {
        match network::udp_peer(id) {
            Ok(Some(ep)) => match network::ipv4_of(&ep) {
                Some(a) => (a, ep.port),
                None => return errno::ENOTCONN,
            },
            Ok(None) => return errno::ENOTCONN,
            Err(e) => return errno_of(e),
        }
    } else {
        match network::udp_local(id) {
            Ok((a, p)) => {
                // A wildcard bind reports the interface address it would use.
                let a = if a.is_unspecified() { network::lease().map_or(a, |l| l.addr) } else { a };
                (a, p)
            }
            Err(e) => return errno_of(e),
        }
    };
    match write_sockaddr_in(addr, port, addr_ptr, len_ptr) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

pub(super) fn setsockopt(level: i32, optname: i32, optval: u64, optlen: u32) -> SyscallResult {
    if level != SOL_SOCKET {
        return ENOPROTOOPT;
    }
    if optlen < 4 || validate_user_buffer(optval, 4).is_err() {
        return errno::EINVAL;
    }
    match optname {
        // Accepted and ignored: no TIME_WAIT/ports to share yet, the buffers
        // are fixed, there is no keepalive on datagrams.
        SO_REUSEADDR | SO_REUSEPORT | SO_BROADCAST | SO_KEEPALIVE | SO_RCVBUF | SO_SNDBUF | SO_LINGER => 0,
        _ => ENOPROTOOPT,
    }
}

pub(super) fn getsockopt(level: i32, optname: i32, optval: u64, optlen_ptr: u64) -> SyscallResult {
    if level != SOL_SOCKET {
        return ENOPROTOOPT;
    }
    if validate_user_buffer(optlen_ptr, 4).is_err() {
        return errno::EFAULT;
    }
    // SAFETY: validated above.
    let cap = unsafe { *(optlen_ptr as *const u32) } as usize;
    if cap < 4 || validate_user_buffer(optval, 4).is_err() {
        return errno::EINVAL;
    }
    let value: i32 = match optname {
        SO_TYPE => SOCK_DGRAM,
        SO_ERROR => 0,
        SO_REUSEADDR | SO_REUSEPORT | SO_BROADCAST | SO_KEEPALIVE => 0,
        SO_RCVBUF | SO_SNDBUF => 32 * 1024,
        _ => return ENOPROTOOPT,
    };
    // SAFETY: both ranges validated.
    unsafe {
        *(optval as *mut i32) = value;
        *(optlen_ptr as *mut u32) = 4;
    }
    0
}

/// Datagram sockets: both directions are always "open"; nothing to tear down.
pub(super) fn shutdown() -> SyscallResult {
    errno::ENOTCONN
}

pub(super) fn unsupported() -> SyscallResult {
    EOPNOTSUPP
}
