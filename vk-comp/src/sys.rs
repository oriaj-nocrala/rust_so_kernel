//! The few C-library calls std does not have (descriptor passing, poll, evdev ioctl, reaping), declared by hand for x86-64 Linux/musl.

use std::ffi::{c_int, c_long, c_void};

#[repr(C)]
pub struct PollFd {
    pub fd: c_int,
    pub events: i16,
    pub revents: i16,
}

#[repr(C)]
pub struct IoVec {
    pub base: *mut c_void,
    pub len: usize,
}

/// `struct msghdr` as the kernel lays it out (musl's padded ints make the same 56 bytes).
#[repr(C)]
pub struct MsgHdr {
    pub name: *mut c_void,
    pub namelen: u32,
    pub _p0: u32,
    pub iov: *mut IoVec,
    pub iovlen: usize,
    pub control: *mut c_void,
    pub controllen: usize,
    pub flags: c_int,
    pub _p1: c_int,
}

#[repr(C)]
struct CmsgHdr {
    len: usize,
    level: c_int,
    kind: c_int,
}

extern "C" {
    pub fn poll(fds: *mut PollFd, n: u64, timeout_ms: c_int) -> c_int;
    pub fn recvmsg(fd: c_int, msg: *mut MsgHdr, flags: c_int) -> c_long;
    pub fn send(fd: c_int, buf: *const c_void, len: usize, flags: c_int) -> c_long;
    pub fn read(fd: c_int, buf: *mut c_void, len: usize) -> c_long;
    pub fn close(fd: c_int) -> c_int;
    pub fn ioctl(fd: c_int, req: u64, ...) -> c_int;
    pub fn open(path: *const i8, flags: c_int, ...) -> c_int;
    pub fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    pub fn signal(sig: c_int, handler: usize) -> usize;
    pub fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
    pub fn munmap(addr: *mut c_void, len: usize) -> c_int;
    pub fn fstat(fd: c_int, st: *mut Stat) -> c_int;
}

/// `struct stat` on x86-64 Linux/musl: `st_size` is at offset 48, 144 bytes in all.
#[repr(C)]
pub struct Stat {
    pub _head: [u8; 48],
    pub st_size: i64,
    pub _tail: [u8; 88],
}

pub const POLLIN: i16 = 1;
pub const POLLHUP: i16 = 0x10;
pub const MSG_DONTWAIT: c_int = 0x40;
pub const MSG_NOSIGNAL: c_int = 0x4000;
pub const WNOHANG: c_int = 1;
pub const O_RDONLY: c_int = 0;
pub const O_NONBLOCK: c_int = 0x800;
pub const EVIOCGRAB: u64 = 0x4004_4590;
pub const SIGINT: c_int = 2;
pub const SIGPIPE: c_int = 13;
pub const SIGTERM: c_int = 15;
pub const SIGTSTP: c_int = 20;
pub const SIG_DFL: usize = 0;
pub const SIG_IGN: usize = 1;
const SOL_SOCKET: c_int = 1;
const SCM_RIGHTS: c_int = 1;

/// One `recvmsg` of `fd`: the bytes read into `buf` and the descriptors that came with them. `Err(errno)`; `Ok((0, []))` is the peer's EOF.
pub fn recv_with_fds(fd: i32, buf: &mut [u8]) -> Result<(usize, Vec<i32>), i32> {
    let mut ctl = [0u64; 24]; // room for 8 descriptors, 8-aligned
    let mut iov = IoVec { base: buf.as_mut_ptr() as *mut c_void, len: buf.len() };
    let mut mh = MsgHdr { name: std::ptr::null_mut(), namelen: 0, _p0: 0, iov: &mut iov, iovlen: 1, control: ctl.as_mut_ptr() as *mut c_void, controllen: std::mem::size_of_val(&ctl), flags: 0, _p1: 0 };
    let n = unsafe { recvmsg(fd, &mut mh, MSG_DONTWAIT) };
    if n < 0 {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    let mut fds = Vec::new();
    // walk the control messages: header 16 bytes, data after it, each padded to 8
    let base = ctl.as_ptr() as *const u8;
    let mut off = 0usize;
    while off + std::mem::size_of::<CmsgHdr>() <= mh.controllen {
        let h = unsafe { std::ptr::read_unaligned(base.add(off) as *const CmsgHdr) };
        if h.len < std::mem::size_of::<CmsgHdr>() || off + h.len > mh.controllen {
            break;
        }
        if h.level == SOL_SOCKET && h.kind == SCM_RIGHTS {
            let count = (h.len - std::mem::size_of::<CmsgHdr>()) / 4;
            for i in 0..count {
                fds.push(unsafe { std::ptr::read_unaligned(base.add(off + std::mem::size_of::<CmsgHdr>() + 4 * i) as *const i32) });
            }
        }
        off += (h.len + 7) & !7;
    }
    Ok((n as usize, fds))
}

/// A pool the client says is `size` bytes, mapped read-only: unmapped when dropped. `None` if the file is smaller (a page past its end would fault
/// this process, not the client).
pub struct Mapping {
    addr: *mut c_void,
    len: usize,
}

impl gui::compositor::PoolMem for Mapping {
    fn as_ptr(&self) -> *const u8 {
        self.addr as *const u8
    }
    fn len(&self) -> usize {
        self.len
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { munmap(self.addr, self.len) };
    }
}

pub fn map_pool(fd: i32, size: usize) -> Option<Mapping> {
    let mut st = Stat { _head: [0; 48], st_size: 0, _tail: [0; 88] };
    if size == 0 || unsafe { fstat(fd, &mut st) } != 0 || (st.st_size as u64) < size as u64 {
        return None;
    }
    let len = (size + 4095) & !4095;
    let a = unsafe { mmap(std::ptr::null_mut(), len, 1, 1, fd, 0) };
    if a as isize == -1 || a.is_null() {
        return None;
    }
    Some(Mapping { addr: a, len })
}
