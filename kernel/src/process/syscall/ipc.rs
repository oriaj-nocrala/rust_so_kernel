// kernel/src/process/syscall/ipc.rs
//
// The AF_UNIX socket syscalls, with their real Linux signatures.
//
// What each one does lives in the host-tested `usock` crate; what lives here
// is the syscall boundary: reading a `struct sockaddr_un` out of user
// memory, walking an `iovec` array, parsing a `SCM_RIGHTS` control message,
// installing descriptors, and deciding between "return EAGAIN" and "park the
// process" for a socket that isn't ready.
//
// ── Blocking ────────────────────────────────────────────────────────────
//
// Every call here follows one shape:
//
//   1. do the work inside `SOCKETS.with(...)`, which disables interrupts;
//   2. let that guard drop, then `dispatch_wakes` whatever it reported;
//   3. if the answer was `Again`: return `-EAGAIN` for an `O_NONBLOCK`
//      socket, otherwise `unix::block_on(...)`, which never returns — the
//      syscall re-executes from scratch when the process wakes (see
//      `ipc/unix.rs`'s module comment for why restarting, and not
//      completing-on-behalf-of, is the right mechanism here).
//
// The steps are kept strictly in that order because the lock order is
// `SOCKETS` → `SCHEDULER`, never the reverse, and because an interrupt
// guard must not be alive across `block_on`'s divergence.

use alloc::boxed::Box;
use alloc::vec::Vec;

use usock::{Shutdown, SockError, SockType, SocketId, UnixAddr, Wakes, SOCKADDR_UN_LEN};

use crate::ipc::unix::{self, SOCKETS};
use crate::process::file::FileHandle;

use super::{errno, validate_user_buffer, SyscallResult};

// ── Constants that cross the ABI boundary ───────────────────────────────

const AF_UNIX: u16 = 1;
const AF_UNSPEC: u16 = 0;

/// `SOCK_NONBLOCK`/`SOCK_CLOEXEC` ride in `socket()`'s type argument.
const SOCK_NONBLOCK: i32 = 0o4000;
const SOCK_CLOEXEC: i32 = 0o2000000;

const MSG_PEEK: u32 = 2;
const MSG_TRUNC: u32 = 0x20;
const MSG_DONTWAIT: u32 = 0x40;
const MSG_CTRUNC: u32 = 8;

const SOL_SOCKET: i32 = 1;
const SO_REUSEADDR: i32 = 2;
const SO_TYPE: i32 = 3;
const SO_ERROR: i32 = 4;
const SO_SNDBUF: i32 = 7;
const SO_RCVBUF: i32 = 8;
const SO_PASSCRED: i32 = 16;
const SO_PEERCRED: i32 = 17;
const SO_ACCEPTCONN: i32 = 30;

const SCM_RIGHTS: i32 = 1;

const EAFNOSUPPORT: i64 = -97;
const EPROTONOSUPPORT: i64 = -93;
const ESOCKTNOSUPPORT: i64 = -94;
const EMFILE: i64 = -24;
const ENOPROTOOPT: i64 = -92;

// ── User-memory structures (Linux x86-64 layout) ────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
struct UserIovec {
    base: u64,
    len: u64,
}

/// `struct msghdr`. `msg_iovlen`/`msg_controllen` are `size_t` on x86-64 —
/// not `int`, which is what this port's `abi-bits/socket.h` used to say.
#[repr(C)]
#[derive(Clone, Copy)]
struct UserMsghdr {
    msg_name: u64,
    msg_namelen: u32,
    _pad0: u32,
    msg_iov: u64,
    msg_iovlen: u64,
    msg_control: u64,
    msg_controllen: u64,
    msg_flags: i32,
    _pad1: u32,
}

/// `struct cmsghdr`: `size_t cmsg_len; int cmsg_level; int cmsg_type;`
#[repr(C)]
#[derive(Clone, Copy)]
struct UserCmsghdr {
    cmsg_len: u64,
    cmsg_level: i32,
    cmsg_type: i32,
}

const CMSG_HDR_LEN: usize = core::mem::size_of::<UserCmsghdr>();

/// `CMSG_ALIGN`: control-message members are `sizeof(size_t)`-aligned.
const fn cmsg_align(n: usize) -> usize {
    (n + 7) & !7
}

// ── sockaddr helpers ────────────────────────────────────────────────────

/// Read and parse a `struct sockaddr_un` from user memory.
fn read_sockaddr(ptr: u64, len: u64) -> Result<UnixAddr, i64> {
    if len < 2 || len as usize > SOCKADDR_UN_LEN {
        return Err(errno::EINVAL);
    }
    validate_user_buffer(ptr, len as usize)?;
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    UnixAddr::parse(bytes).map_err(unix::errno_of)
}

/// Family field of a user `sockaddr`, without parsing the rest — `connect()`
/// treats `AF_UNSPEC` specially (it means "disconnect").
fn peek_family(ptr: u64, len: u64) -> Option<u16> {
    if len < 2 || validate_user_buffer(ptr, 2).is_err() {
        return None;
    }
    let b = unsafe { core::slice::from_raw_parts(ptr as *const u8, 2) };
    Some(u16::from_le_bytes([b[0], b[1]]))
}

/// Write an address back out to a user `sockaddr` + `socklen_t*` pair, the
/// way `getsockname`/`getpeername`/`accept`/`recvfrom` all do: copy at most
/// what the caller's buffer holds, but report the address's true length so
/// truncation is detectable. A null pointer means "caller doesn't care".
fn write_sockaddr(addr: &UnixAddr, ptr: u64, len_ptr: u64) -> Result<(), i64> {
    if ptr == 0 || len_ptr == 0 {
        return Ok(());
    }
    validate_user_buffer(len_ptr, 4)?;
    let cap = unsafe { *(len_ptr as *const u32) } as usize;
    let cap = cap.min(SOCKADDR_UN_LEN);
    if cap > 0 {
        validate_user_buffer(ptr, cap)?;
    }

    let mut buf = [0u8; SOCKADDR_UN_LEN];
    let full = addr.encode(&mut buf);
    let copy = full.min(cap);
    unsafe {
        core::ptr::copy_nonoverlapping(buf.as_ptr(), ptr as *mut u8, copy);
        *(len_ptr as *mut u32) = full as u32;
    }
    Ok(())
}

/// A pathname address is resolved against the caller's cwd, so a relative
/// `bind("sock")` means what the shell would mean by it.
fn normalized(addr: UnixAddr) -> UnixAddr {
    match addr {
        UnixAddr::Path(_) => unix::normalize(addr, &super::current_cwd()),
        other => other,
    }
}

// ── socket / socketpair ─────────────────────────────────────────────────

fn parse_type(ty: i32) -> Result<(SockType, bool), i64> {
    let base = ty & !(SOCK_NONBLOCK | SOCK_CLOEXEC);
    let kind = match base {
        1 => SockType::Stream,
        2 => SockType::Dgram,
        _ => return Err(ESOCKTNOSUPPORT),
    };
    Ok((kind, ty & SOCK_NONBLOCK != 0))
}

fn check_domain(domain: i32, protocol: i32) -> Result<(), i64> {
    if domain != AF_UNIX as i32 {
        return Err(EAFNOSUPPORT);
    }
    if protocol != 0 {
        return Err(EPROTONOSUPPORT);
    }
    Ok(())
}

pub(super) fn sys_socket(domain: i32, ty: i32, protocol: i32) -> SyscallResult {
    if let Err(e) = check_domain(domain, protocol) {
        return e;
    }
    let (kind, nonblock) = match parse_type(ty) {
        Ok(v) => v,
        Err(e) => return e,
    };

    let id = match SOCKETS.with(|t| t.create(kind)) {
        Ok(id) => id,
        Err(e) => return unix::errno_of(e),
    };

    match install_fd(id, nonblock) {
        Ok(fd) => fd as i64,
        Err(e) => e,
    }
}

pub(super) fn sys_socketpair(domain: i32, ty: i32, protocol: i32, sv: u64) -> SyscallResult {
    if let Err(e) = check_domain(domain, protocol) {
        return e;
    }
    let (kind, nonblock) = match parse_type(ty) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if let Err(e) = validate_user_buffer(sv, 8) {
        return e;
    }

    let (a, b) = match SOCKETS.with(|t| t.socketpair(kind)) {
        Ok(p) => p,
        Err(e) => return unix::errno_of(e),
    };

    let fd_a = match install_fd(a, nonblock) {
        Ok(fd) => fd,
        Err(e) => {
            // Neither socket ever reached an fd table, so nothing else will
            // ever drop their references.
            release_orphan(a);
            release_orphan(b);
            return e;
        }
    };
    let fd_b = match install_fd(b, nonblock) {
        Ok(fd) => fd,
        Err(e) => {
            close_fd(fd_a);
            release_orphan(b);
            return e;
        }
    };

    unsafe {
        let p = sv as *mut i32;
        *p = fd_a;
        *p.add(1) = fd_b;
    }
    0
}

// ── bind / listen ───────────────────────────────────────────────────────

pub(super) fn sys_bind(fd: i32, addr_ptr: u64, addrlen: u64) -> SyscallResult {
    let id = match unix::socket_of_fd(fd) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let addr = match read_sockaddr(addr_ptr, addrlen) {
        Ok(a) => normalized(a),
        Err(e) => return e,
    };

    // A pathname bind creates a real filesystem node first: it is what makes
    // the name visible to `ls`, resolvable by a `connect()` from another
    // process, and removable with `unlink` — and its EEXIST is what makes a
    // second bind to the same path fail even after the first socket died.
    let created_node = match &addr {
        UnixAddr::Path(p) => {
            if let Err(e) = unix::create_path_node(p) {
                return e;
            }
            true
        }
        _ => false,
    };

    match SOCKETS.with(|t| t.bind(id, addr.clone())) {
        Ok(()) => 0,
        Err(e) => {
            // Don't leave a node behind for a bind that didn't take.
            if created_node {
                if let UnixAddr::Path(p) = &addr {
                    let _ = crate::fs::vfs::unlink(p);
                }
            }
            unix::errno_of(e)
        }
    }
}

pub(super) fn sys_listen(fd: i32, backlog: i32) -> SyscallResult {
    let id = match unix::socket_of_fd(fd) {
        Ok(id) => id,
        Err(e) => return e,
    };
    match SOCKETS.with(|t| t.listen(id, backlog)) {
        Ok(()) => 0,
        Err(e) => unix::errno_of(e),
    }
}

// ── connect / accept ────────────────────────────────────────────────────

pub(super) fn sys_connect(fd: i32, addr_ptr: u64, addrlen: u64) -> SyscallResult {
    let id = match unix::socket_of_fd(fd) {
        Ok(id) => id,
        Err(e) => return e,
    };

    // connect(fd, AF_UNSPEC) dissolves a datagram socket's default
    // destination — the documented way to undo a datagram connect().
    if peek_family(addr_ptr, addrlen) == Some(AF_UNSPEC) {
        return match SOCKETS.with(|t| t.disconnect(id)) {
            Ok(()) => 0,
            Err(e) => unix::errno_of(e),
        };
    }

    let addr = match read_sockaddr(addr_ptr, addrlen) {
        Ok(a) => normalized(a),
        Err(e) => return e,
    };

    // "No such file" and "nobody is listening" are different answers, and
    // userspace relies on telling them apart.
    if let UnixAddr::Path(p) = &addr {
        if !unix::path_node_exists(p) {
            return errno::ENOENT;
        }
    }

    let (result, wakes) = SOCKETS.with(|t| {
        let r = t.connect(id, &addr);
        let w = match &r {
            Ok(o) => o.wakes.clone(),
            Err(_) => Wakes::default(),
        };
        (r.map(|_| ()), w)
    });
    unix::dispatch_wakes(&wakes);

    match result {
        Ok(()) => 0,
        // A full backlog is the one blocking case: wait on the listener,
        // which becomes writable again as soon as it accepts something.
        Err(SockError::Again) => {
            if unix::fd_is_nonblocking(fd) {
                return errno::EAGAIN;
            }
            match SOCKETS.with(|t| t.lookup(&addr)) {
                Some(listener) => unix::block_on(listener),
                None => unix::errno_of(SockError::ConnRefused),
            }
        }
        Err(e) => unix::errno_of(e),
    }
}

pub(super) fn sys_accept4(fd: i32, addr_ptr: u64, len_ptr: u64, flags: i32) -> SyscallResult {
    let id = match unix::socket_of_fd(fd) {
        Ok(id) => id,
        Err(e) => return e,
    };

    let (result, wakes) = SOCKETS.with(|t| {
        let r = t.accept(id);
        let w = match &r {
            Ok(o) => o.wakes.clone(),
            Err(_) => Wakes::default(),
        };
        (r.map(|o| (o.id, o.peer_addr)), w)
    });
    // Accepting frees a backlog slot — a connect() blocked on a full one is
    // waiting for exactly this.
    unix::dispatch_wakes(&wakes);

    let (child, peer_addr) = match result {
        Ok(v) => v,
        Err(SockError::Again) => {
            if unix::fd_is_nonblocking(fd) || flags & SOCK_NONBLOCK != 0 {
                return errno::EAGAIN;
            }
            unix::block_on(id);
        }
        Err(e) => return unix::errno_of(e),
    };

    if let Err(e) = write_sockaddr(&peer_addr, addr_ptr, len_ptr) {
        release_orphan(child);
        return e;
    }

    match install_fd(child, flags & SOCK_NONBLOCK != 0) {
        Ok(new_fd) => new_fd as i64,
        Err(e) => e,
    }
}

// ── send / recv ─────────────────────────────────────────────────────────

pub(super) fn sys_sendto(
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
        match read_sockaddr(dest_ptr, dest_len) {
            Ok(a) => Some(normalized(a)),
            Err(e) => return e,
        }
    } else {
        None
    };

    let data = unsafe { user_slice(buf, len) };
    send_common(fd, data, Vec::new(), dest.as_ref(), flags)
}

pub(super) fn sys_recvfrom(
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

    let out = match recv_common(fd, unsafe { user_slice_mut(buf, len) }, flags) {
        Ok(o) => o,
        Err(e) => return e,
    };

    // Ancillary descriptors have nowhere to go in a recvfrom(); closing them
    // is what Linux does too, rather than losing the underlying file.
    drop(out.fds);

    if let Some(from) = &out.from {
        if let Err(e) = write_sockaddr(from, src_ptr, src_len_ptr) {
            return e;
        }
    } else if src_len_ptr != 0 && validate_user_buffer(src_len_ptr, 4).is_ok() {
        unsafe { *(src_len_ptr as *mut u32) = 0 };
    }

    if flags & MSG_TRUNC != 0 {
        out.full_len as i64
    } else {
        out.n as i64
    }
}

pub(super) fn sys_sendmsg(fd: i32, msg_ptr: u64, flags: u32) -> SyscallResult {
    let msg = match read_msghdr(msg_ptr) {
        Ok(m) => m,
        Err(e) => return e,
    };

    let data = match gather_iovecs(&msg) {
        Ok(d) => d,
        Err(e) => return e,
    };

    let dest = if msg.msg_name != 0 && msg.msg_namelen >= 2 {
        match read_sockaddr(msg.msg_name, msg.msg_namelen as u64) {
            Ok(a) => Some(normalized(a)),
            Err(e) => return e,
        }
    } else {
        None
    };

    // SCM_RIGHTS: pull the named descriptors out of the sender's table now,
    // so they travel as owned handles. A send that fails hands them back and
    // they are simply dropped here — the sender still holds its own fds.
    let fds = match collect_scm_rights(&msg) {
        Ok(f) => f,
        Err(e) => return e,
    };

    send_common(fd, &data, fds, dest.as_ref(), flags)
}

pub(super) fn sys_recvmsg(fd: i32, msg_ptr: u64, flags: u32) -> SyscallResult {
    let msg = match read_msghdr(msg_ptr) {
        Ok(m) => m,
        Err(e) => return e,
    };

    // One contiguous staging buffer, scattered into the iovecs afterwards.
    let total = match iovec_total(&msg) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let mut staging = alloc::vec![0u8; total];

    let out = match recv_common(fd, &mut staging, flags) {
        Ok(o) => o,
        Err(e) => return e,
    };

    if let Err(e) = scatter_iovecs(&msg, &staging[..out.n]) {
        return e;
    }

    let mut msg_flags = 0i32;
    if out.full_len > out.n {
        msg_flags |= MSG_TRUNC as i32;
    }

    // Install received descriptors and describe them in the control buffer.
    if !out.fds.is_empty() {
        match install_scm_rights(&msg, out.fds) {
            Ok(truncated) => {
                if truncated {
                    msg_flags |= MSG_CTRUNC as i32;
                }
            }
            Err(e) => return e,
        }
    } else if msg.msg_controllen > 0 {
        if let Err(e) = set_controllen(msg_ptr, 0) {
            return e;
        }
    }

    if let Some(from) = &out.from {
        if let Err(e) = write_sockaddr(from, msg.msg_name, msg_ptr + 8) {
            return e;
        }
    }

    if let Err(e) = set_msg_flags(msg_ptr, msg_flags) {
        return e;
    }

    if flags & MSG_TRUNC != 0 {
        out.full_len as i64
    } else {
        out.n as i64
    }
}

// ── shutdown / names ────────────────────────────────────────────────────

pub(super) fn sys_shutdown(fd: i32, how: i32) -> SyscallResult {
    let id = match unix::socket_of_fd(fd) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let how = match Shutdown::from_raw(how) {
        Some(h) => h,
        None => return errno::EINVAL,
    };

    let (result, wakes) = SOCKETS.with(|t| match t.shutdown(id, how) {
        Ok(w) => (Ok(()), w),
        Err(e) => (Err(e), Wakes::default()),
    });
    unix::dispatch_wakes(&wakes);

    match result {
        Ok(()) => 0,
        Err(e) => unix::errno_of(e),
    }
}

pub(super) fn sys_getsockname(fd: i32, addr_ptr: u64, len_ptr: u64) -> SyscallResult {
    name_of(fd, addr_ptr, len_ptr, false)
}

pub(super) fn sys_getpeername(fd: i32, addr_ptr: u64, len_ptr: u64) -> SyscallResult {
    name_of(fd, addr_ptr, len_ptr, true)
}

fn name_of(fd: i32, addr_ptr: u64, len_ptr: u64, peer: bool) -> SyscallResult {
    let id = match unix::socket_of_fd(fd) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let addr = SOCKETS.with(|t| if peer { t.peername(id) } else { t.sockname(id) });
    match addr {
        Ok(a) => match write_sockaddr(&a, addr_ptr, len_ptr) {
            Ok(()) => 0,
            Err(e) => e,
        },
        Err(e) => unix::errno_of(e),
    }
}

// ── socket options ──────────────────────────────────────────────────────

pub(super) fn sys_setsockopt(
    fd: i32,
    level: i32,
    optname: i32,
    optval: u64,
    optlen: u32,
) -> SyscallResult {
    let id = match unix::socket_of_fd(fd) {
        Ok(id) => id,
        Err(e) => return e,
    };
    if level != SOL_SOCKET {
        return ENOPROTOOPT;
    }
    if optlen < 4 || validate_user_buffer(optval, 4).is_err() {
        return errno::EINVAL;
    }
    let val = unsafe { *(optval as *const i32) };

    match optname {
        SO_SNDBUF => {
            let _ = SOCKETS.with(|t| t.set_sndbuf(id, val.max(0) as usize));
            0
        }
        SO_RCVBUF => {
            let _ = SOCKETS.with(|t| t.set_rcvbuf(id, val.max(0) as usize));
            0
        }
        // Accepted and ignored: this kernel has no uid model, so credential
        // passing has nothing to pass, and an AF_UNIX address is never
        // "already in use" in the TIME_WAIT sense SO_REUSEADDR exists for.
        SO_REUSEADDR | SO_PASSCRED => 0,
        _ => ENOPROTOOPT,
    }
}

pub(super) fn sys_getsockopt(
    fd: i32,
    level: i32,
    optname: i32,
    optval: u64,
    optlen_ptr: u64,
) -> SyscallResult {
    let id = match unix::socket_of_fd(fd) {
        Ok(id) => id,
        Err(e) => return e,
    };
    if level != SOL_SOCKET {
        return ENOPROTOOPT;
    }
    if validate_user_buffer(optlen_ptr, 4).is_err() {
        return errno::EFAULT;
    }
    let cap = unsafe { *(optlen_ptr as *const u32) } as usize;
    if cap < 4 || validate_user_buffer(optval, 4).is_err() {
        return errno::EINVAL;
    }

    let value: i32 = match optname {
        SO_TYPE => match SOCKETS.with(|t| t.sock_type(id)) {
            Ok(SockType::Stream) => 1,
            Ok(SockType::Dgram) => 2,
            Err(e) => return unix::errno_of(e),
        },
        SO_ERROR => match SOCKETS.with(|t| t.take_error(id)) {
            Ok(v) => v,
            Err(e) => return unix::errno_of(e),
        },
        SO_SNDBUF => match SOCKETS.with(|t| t.sndbuf(id)) {
            Ok(v) => v as i32,
            Err(e) => return unix::errno_of(e),
        },
        SO_RCVBUF => match SOCKETS.with(|t| t.rcvbuf(id)) {
            Ok(v) => v as i32,
            Err(e) => return unix::errno_of(e),
        },
        SO_ACCEPTCONN => SOCKETS.with(|t| t.get(id).map(|s| s.is_listening()).unwrap_or(false)) as i32,
        // SO_PEERCRED would report the peer's uid/pid; there is no uid model
        // here, and inventing one silently is worse than saying no.
        SO_PEERCRED => return ENOPROTOOPT,
        _ => return ENOPROTOOPT,
    };

    unsafe {
        *(optval as *mut i32) = value;
        *(optlen_ptr as *mut u32) = 4;
    }
    0
}

// ── shared send/recv bodies ─────────────────────────────────────────────

struct RecvResult {
    n: usize,
    full_len: usize,
    fds: Vec<Box<dyn FileHandle>>,
    from: Option<UnixAddr>,
}

fn send_common(
    fd: i32,
    data: &[u8],
    mut fds: Vec<Box<dyn FileHandle>>,
    dest: Option<&UnixAddr>,
    flags: u32,
) -> SyscallResult {
    let id = match unix::socket_of_fd(fd) {
        Ok(id) => id,
        Err(e) => return e,
    };

    let (result, wakes) = SOCKETS.with(|t| {
        let r = t.send(id, data, &mut fds, dest);
        let w = match &r {
            Ok(o) => o.wakes.clone(),
            Err(_) => Wakes::default(),
        };
        (r.map(|o| o.written), w)
    });
    unix::dispatch_wakes(&wakes);

    match result {
        Ok(n) => n as i64,
        Err(SockError::Again) => {
            if flags & MSG_DONTWAIT != 0 || unix::fd_is_nonblocking(fd) {
                return errno::EAGAIN;
            }
            // `fds` still holds the descriptors the send didn't take; they
            // drop here, which is correct — the retry re-reads them from the
            // sender's fd table, which still has them open.
            drop(fds);
            unix::block_on(peer_or_self(id))
        }
        Err(e) => unix::errno_of(e),
    }
}

fn recv_common(fd: i32, buf: &mut [u8], flags: u32) -> Result<RecvResult, i64> {
    let id = match unix::socket_of_fd(fd) {
        Ok(id) => id,
        Err(e) => return Err(e),
    };
    let peek = flags & MSG_PEEK != 0;

    let (result, wakes) = SOCKETS.with(|t| {
        let r = t.recv(id, buf, peek);
        let w = match &r {
            Ok(o) => o.wakes.clone(),
            Err(_) => Wakes::default(),
        };
        (r.map(|o| (o.n, o.full_len, o.fds, o.from)), w)
    });
    unix::dispatch_wakes(&wakes);

    match result {
        Ok((n, full_len, fds, from)) => Ok(RecvResult { n, full_len, fds, from }),
        Err(SockError::Again) => {
            if flags & MSG_DONTWAIT != 0 || unix::fd_is_nonblocking(fd) {
                return Err(errno::EAGAIN);
            }
            unix::block_on(id)
        }
        Err(e) => Err(unix::errno_of(e)),
    }
}

/// Which socket a blocked sender should wait on: the *peer's* queue is what
/// has to drain, so that is what gets woken when a reader consumes bytes.
fn peer_or_self(id: SocketId) -> SocketId {
    SOCKETS.with(|t| t.get(id).and_then(|s| s.peer_id()).unwrap_or(id))
}

// ── iovec / msghdr plumbing ─────────────────────────────────────────────

unsafe fn user_slice<'a>(ptr: u64, len: usize) -> &'a [u8] {
    if len == 0 {
        return &[];
    }
    unsafe { core::slice::from_raw_parts(ptr as *const u8, len) }
}

unsafe fn user_slice_mut<'a>(ptr: u64, len: usize) -> &'a mut [u8] {
    if len == 0 {
        return &mut [];
    }
    unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, len) }
}

fn read_msghdr(ptr: u64) -> Result<UserMsghdr, i64> {
    validate_user_buffer(ptr, core::mem::size_of::<UserMsghdr>())?;
    Ok(unsafe { core::ptr::read_unaligned(ptr as *const UserMsghdr) })
}

/// `msghdr.msg_flags` sits at a fixed offset; only that field is written
/// back, never the whole struct (the caller owns the rest).
fn set_msg_flags(ptr: u64, flags: i32) -> Result<(), i64> {
    let off = core::mem::offset_of!(UserMsghdr, msg_flags) as u64;
    validate_user_buffer(ptr + off, 4)?;
    unsafe { *((ptr + off) as *mut i32) = flags };
    Ok(())
}

fn set_controllen(ptr: u64, len: u64) -> Result<(), i64> {
    let off = core::mem::offset_of!(UserMsghdr, msg_controllen) as u64;
    validate_user_buffer(ptr + off, 8)?;
    unsafe { *((ptr + off) as *mut u64) = len };
    Ok(())
}

const MAX_IOV: usize = 16;

fn read_iovecs(msg: &UserMsghdr) -> Result<Vec<UserIovec>, i64> {
    let n = msg.msg_iovlen as usize;
    if n == 0 {
        return Ok(Vec::new());
    }
    if n > MAX_IOV {
        return Err(errno::EINVAL);
    }
    validate_user_buffer(msg.msg_iov, n * core::mem::size_of::<UserIovec>())?;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let iov = unsafe {
            core::ptr::read_unaligned((msg.msg_iov as *const UserIovec).add(i))
        };
        if iov.len > 0 {
            validate_user_buffer(iov.base, iov.len as usize)?;
        }
        out.push(iov);
    }
    Ok(out)
}

fn iovec_total(msg: &UserMsghdr) -> Result<usize, i64> {
    Ok(read_iovecs(msg)?.iter().map(|v| v.len as usize).sum())
}

fn gather_iovecs(msg: &UserMsghdr) -> Result<Vec<u8>, i64> {
    let iovs = read_iovecs(msg)?;
    let total: usize = iovs.iter().map(|v| v.len as usize).sum();
    let mut out = Vec::with_capacity(total);
    for iov in iovs {
        out.extend_from_slice(unsafe { user_slice(iov.base, iov.len as usize) });
    }
    Ok(out)
}

fn scatter_iovecs(msg: &UserMsghdr, data: &[u8]) -> Result<(), i64> {
    let iovs = read_iovecs(msg)?;
    let mut off = 0usize;
    for iov in iovs {
        if off >= data.len() {
            break;
        }
        let n = (iov.len as usize).min(data.len() - off);
        unsafe {
            core::ptr::copy_nonoverlapping(data[off..].as_ptr(), iov.base as *mut u8, n);
        }
        off += n;
    }
    Ok(())
}

// ── SCM_RIGHTS ──────────────────────────────────────────────────────────

const MAX_SCM_FDS: usize = 8;

/// Take the descriptors named by a `SCM_RIGHTS` control message out of the
/// sender's fd table — as *duplicates*, so the sender keeps its own fds
/// open, exactly like Linux.
fn collect_scm_rights(msg: &UserMsghdr) -> Result<Vec<Box<dyn FileHandle>>, i64> {
    let clen = msg.msg_controllen as usize;
    if msg.msg_control == 0 || clen < CMSG_HDR_LEN {
        return Ok(Vec::new());
    }
    validate_user_buffer(msg.msg_control, clen)?;

    let mut out: Vec<Box<dyn FileHandle>> = Vec::new();
    let mut off = 0usize;
    while off + CMSG_HDR_LEN <= clen {
        let hdr = unsafe {
            core::ptr::read_unaligned((msg.msg_control + off as u64) as *const UserCmsghdr)
        };
        let len = hdr.cmsg_len as usize;
        if len < CMSG_HDR_LEN || off + len > clen {
            return Err(errno::EINVAL);
        }
        if hdr.cmsg_level == SOL_SOCKET && hdr.cmsg_type == SCM_RIGHTS {
            let count = (len - CMSG_HDR_LEN) / 4;
            if out.len() + count > MAX_SCM_FDS {
                return Err(errno::EINVAL);
            }
            let base = msg.msg_control + (off + CMSG_HDR_LEN) as u64;
            let files = current_files();
            for i in 0..count {
                let fd = unsafe { *((base as *const i32).add(i)) };
                let dup = {
                    let guard = files.lock();
                    match guard.get(fd as usize) {
                        Ok(h) => h.dup(),
                        Err(_) => return Err(errno::EBADF),
                    }
                };
                match dup {
                    Some(h) => out.push(h),
                    // A handle that cannot be duplicated cannot be passed;
                    // saying so beats sending half the set.
                    None => return Err(errno::EINVAL),
                }
            }
        }
        off += cmsg_align(len);
    }
    Ok(out)
}

/// Install received descriptors into this process's fd table and write the
/// resulting fd numbers into the caller's control buffer.
///
/// Returns true if the control buffer was too small to describe them all
/// (`MSG_CTRUNC`), in which case the descriptors that didn't fit are closed
/// rather than leaked into a process that can never name them.
fn install_scm_rights(msg: &UserMsghdr, fds: Vec<Box<dyn FileHandle>>) -> Result<bool, i64> {
    let clen = msg.msg_controllen as usize;
    if msg.msg_control == 0 || clen < CMSG_HDR_LEN + 4 {
        return Ok(true); // nowhere to report them: everything is truncated
    }
    validate_user_buffer(msg.msg_control, clen)?;

    let room = (clen - CMSG_HDR_LEN) / 4;
    let take = room.min(fds.len());
    let truncated = take < fds.len();

    let files = current_files();
    let mut numbers: Vec<i32> = Vec::with_capacity(take);
    for handle in fds.into_iter().take(take) {
        let allocated = files.lock().allocate(handle);
        match allocated {
            Ok(fd) => numbers.push(fd as i32),
            // Out of descriptors: the rest are dropped (closed), which is
            // what Linux does when the receiver's table is full.
            Err(_) => break,
        }
    }

    let payload = numbers.len() * 4;
    let hdr = UserCmsghdr {
        cmsg_len: (CMSG_HDR_LEN + payload) as u64,
        cmsg_level: SOL_SOCKET,
        cmsg_type: SCM_RIGHTS,
    };
    unsafe {
        core::ptr::write_unaligned(msg.msg_control as *mut UserCmsghdr, hdr);
        let base = (msg.msg_control + CMSG_HDR_LEN as u64) as *mut i32;
        for (i, fd) in numbers.iter().enumerate() {
            core::ptr::write_unaligned(base.add(i), *fd);
        }
    }
    Ok(truncated)
}

// ── fd-table plumbing ───────────────────────────────────────────────────

fn current_files() -> alloc::sync::Arc<spin::Mutex<crate::process::file::FileDescriptorTable>> {
    let guard = crate::process::irq_guard::SchedGuard::lock();
    guard
        .running_ref()
        .map(|p| p.files.clone())
        .unwrap_or_else(|| alloc::sync::Arc::new(spin::Mutex::new(
            crate::process::file::FileDescriptorTable::new(),
        )))
}

/// Put a socket behind a new fd, applying `O_NONBLOCK` if asked.
fn install_fd(id: SocketId, nonblock: bool) -> Result<i32, i64> {
    let handle = crate::ipc::unix::UnixSocketHandle::new(id);
    handle.set_nonblocking(nonblock);
    let files = current_files();
    let allocated = files.lock().allocate(Box::new(handle));
    match allocated {
        Ok(fd) => Ok(fd as i32),
        Err(_) => Err(EMFILE), // the dropped handle already released the socket
    }
}

/// Release a socket that never reached an fd table (so nothing else will
/// ever drop a reference to it).
fn release_orphan(id: SocketId) {
    let (fds, wakes) = SOCKETS.with(|t| {
        let out = t.close(id);
        (out.fds, out.wakes)
    });
    drop(fds);
    unix::dispatch_wakes(&wakes);
}

fn close_fd(fd: i32) {
    let files = current_files();
    let _ = files.lock().close(fd as usize);
}
