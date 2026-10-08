//! The C-library calls std does not wrap (descriptor passing, poll, memfd, mmap), declared by hand for x86-64 Linux: the same declarations
//! link against musl in the guest and glibc on the host, where the tests run.

use std::ffi::{c_int, c_long, c_uint, c_void};
use std::io;

#[repr(C)]
pub struct PollFd {
    pub fd: c_int,
    pub events: i16,
    pub revents: i16,
}

#[repr(C)]
struct IoVec {
    base: *mut c_void,
    len: usize,
}

/// `struct msghdr` as the kernel lays it out (musl's padded ints make the same 56 bytes).
#[repr(C)]
struct MsgHdr {
    name: *mut c_void,
    namelen: u32,
    _p0: u32,
    iov: *mut IoVec,
    iovlen: usize,
    control: *mut c_void,
    controllen: usize,
    flags: c_int,
    _p1: c_int,
}

/// `struct cmsghdr`: 16 bytes, the data follows, each message padded to 8.
#[repr(C)]
struct CmsgHdr {
    len: usize,
    level: c_int,
    kind: c_int,
}

extern "C" {
    fn poll(fds: *mut PollFd, n: u64, timeout_ms: c_int) -> c_int;
    fn sendmsg(fd: c_int, msg: *const MsgHdr, flags: c_int) -> c_long;
    fn recvmsg(fd: c_int, msg: *mut MsgHdr, flags: c_int) -> c_long;
    fn memfd_create(name: *const i8, flags: c_uint) -> c_int;
    fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
}

pub const POLLIN: i16 = 1;
const SOL_SOCKET: c_int = 1;
const SCM_RIGHTS: c_int = 1;
const MSG_NOSIGNAL: c_int = 0x4000;
const PROT_READ: c_int = 1;
const PROT_WRITE: c_int = 2;
const MAP_SHARED: c_int = 1;
const EINTR: i32 = 4;

/// Waits up to `timeout_ms` (-1: forever) for `fd` to be readable (or hung up). `Ok(false)` on timeout.
pub fn wait_readable(fd: i32, timeout_ms: i32) -> io::Result<bool> {
    loop {
        let mut p = PollFd { fd, events: POLLIN, revents: 0 };
        let n = unsafe { poll(&mut p, 1, timeout_ms) };
        if n >= 0 {
            return Ok(n > 0);
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(EINTR) {
            return Err(e);
        }
    }
}

/// Sends all of `bytes` on the stream socket `fd`, with `fds` (as `SCM_RIGHTS`) on the first piece. No `SIGPIPE`: a closed peer is `EPIPE`.
pub fn send_with_fds(fd: i32, mut bytes: &[u8], fds: &[i32]) -> io::Result<()> {
    let mut ctl = [0u64; 2 + 16]; // header + up to 32 descriptors, 8-aligned
    assert!(fds.len() <= 32, "too many descriptors in one message");
    let mut first = true;
    while !bytes.is_empty() || (first && !fds.is_empty()) {
        let mut iov = IoVec { base: bytes.as_ptr() as *mut c_void, len: bytes.len() };
        let mut mh = MsgHdr {
            name: std::ptr::null_mut(),
            namelen: 0,
            _p0: 0,
            iov: &mut iov,
            iovlen: 1,
            control: std::ptr::null_mut(),
            controllen: 0,
            flags: 0,
            _p1: 0,
        };
        if first && !fds.is_empty() {
            let len = std::mem::size_of::<CmsgHdr>() + 4 * fds.len();
            let base = ctl.as_mut_ptr() as *mut u8;
            unsafe {
                std::ptr::write_unaligned(base as *mut CmsgHdr, CmsgHdr { len, level: SOL_SOCKET, kind: SCM_RIGHTS });
                for (i, &f) in fds.iter().enumerate() {
                    std::ptr::write_unaligned(base.add(std::mem::size_of::<CmsgHdr>() + 4 * i) as *mut i32, f);
                }
            }
            mh.control = ctl.as_mut_ptr() as *mut c_void;
            mh.controllen = (len + 7) & !7;
        }
        let n = unsafe { sendmsg(fd, &mh, MSG_NOSIGNAL) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(EINTR) {
                continue;
            }
            return Err(e);
        }
        first = false;
        bytes = &bytes[n as usize..];
    }
    Ok(())
}

/// One `recvmsg` of `fd`: the bytes read into `buf` and the descriptors that came with them. `Ok((0, []))` is the peer's EOF.
pub fn recv_with_fds(fd: i32, buf: &mut [u8]) -> io::Result<(usize, Vec<i32>)> {
    let mut ctl = [0u64; 24]; // room for 8 descriptors, 8-aligned
    let mut iov = IoVec { base: buf.as_mut_ptr() as *mut c_void, len: buf.len() };
    let mut mh = MsgHdr {
        name: std::ptr::null_mut(),
        namelen: 0,
        _p0: 0,
        iov: &mut iov,
        iovlen: 1,
        control: ctl.as_mut_ptr() as *mut c_void,
        controllen: std::mem::size_of_val(&ctl),
        flags: 0,
        _p1: 0,
    };
    let n = loop {
        let n = unsafe { recvmsg(fd, &mut mh, 0) };
        if n >= 0 {
            break n;
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(EINTR) {
            return Err(e);
        }
    };
    let mut fds = Vec::new();
    let base = ctl.as_ptr() as *const u8;
    let hdr = std::mem::size_of::<CmsgHdr>();
    let mut off = 0usize;
    while off + hdr <= mh.controllen {
        let h = unsafe { std::ptr::read_unaligned(base.add(off) as *const CmsgHdr) };
        if h.len < hdr || off + h.len > mh.controllen {
            break;
        }
        if h.level == SOL_SOCKET && h.kind == SCM_RIGHTS {
            for i in 0..(h.len - hdr) / 4 {
                fds.push(unsafe { std::ptr::read_unaligned(base.add(off + hdr + 4 * i) as *const i32) });
            }
        }
        off += (h.len + 7) & !7;
    }
    Ok((n as usize, fds))
}

/// An anonymous shared-memory file (`memfd_create`).
pub fn memfd(name: &std::ffi::CStr) -> io::Result<i32> {
    let fd = unsafe { memfd_create(name.as_ptr(), 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// `len` bytes of `fd` from offset 0, shared and read-write. Unmapped when dropped.
pub struct Mapping {
    addr: *mut c_void,
    len: usize,
}

impl Mapping {
    pub fn new(fd: i32, len: usize) -> io::Result<Mapping> {
        let a = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0) };
        if a as isize == -1 || a.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Mapping { addr: a, len })
    }

    /// The mapping as `0x00RRGGBB` pixels (`len / 4` of them).
    pub fn pixels(&mut self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.addr as *mut u32, self.len / 4) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { munmap(self.addr, self.len) };
    }
}
