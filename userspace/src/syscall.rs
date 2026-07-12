//! Raw syscall ABI for this kernel.
//!
//! Matches `kernel/src/process/syscall.rs::SyscallNumber` and the
//! `syscall_entry_fast` calling convention exactly: entered via the
//! `syscall` instruction, args in rdi/rsi/rdx/r10/r8/r9, return value
//! (single register, negative = -errno) in rax. rcx/r11 are clobbered
//! by the `syscall` instruction itself.

use core::arch::asm;

#[inline(always)]
unsafe fn syscall0(nr: u64) -> i64 {
    let ret: i64;
    asm!("syscall", inlateout("rax") nr as i64 => ret,
        out("rcx") _, out("r11") _, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall1(nr: u64, a1: u64) -> i64 {
    let ret: i64;
    asm!("syscall", inlateout("rax") nr as i64 => ret,
        in("rdi") a1, out("rcx") _, out("r11") _, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall2(nr: u64, a1: u64, a2: u64) -> i64 {
    let ret: i64;
    asm!("syscall", inlateout("rax") nr as i64 => ret,
        in("rdi") a1, in("rsi") a2, out("rcx") _, out("r11") _, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall3(nr: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret: i64;
    asm!("syscall", inlateout("rax") nr as i64 => ret,
        in("rdi") a1, in("rsi") a2, in("rdx") a3,
        out("rcx") _, out("r11") _, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall4(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> i64 {
    let ret: i64;
    asm!("syscall", inlateout("rax") nr as i64 => ret,
        in("rdi") a1, in("rsi") a2, in("rdx") a3, in("r10") a4,
        out("rcx") _, out("r11") _, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall5(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let ret: i64;
    asm!("syscall", inlateout("rax") nr as i64 => ret,
        in("rdi") a1, in("rsi") a2, in("rdx") a3, in("r10") a4, in("r8") a5,
        out("rcx") _, out("r11") _, options(nostack));
    ret
}

// ── Syscall numbers (must match kernel/src/process/syscall.rs::SyscallNumber) ──

const SYS_READ: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_OPEN: u64 = 2;
const SYS_CLOSE: u64 = 3;
const SYS_STAT: u64 = 4;
const SYS_FSTAT: u64 = 5;
#[allow(dead_code)]
const SYS_POLL: u64 = 7;
#[allow(dead_code)]
const SYS_LSEEK: u64 = 8;
const SYS_MMAP: u64 = 9;
#[allow(dead_code)]
const SYS_MUNMAP: u64 = 11;
#[allow(dead_code)]
const SYS_YIELD: u64 = 24;
const SYS_NANOSLEEP: u64 = 35;
const SYS_GETPID: u64 = 39;
const SYS_SOCKET: u64 = 41;
const SYS_CONNECT: u64 = 42;
const SYS_ACCEPT: u64 = 43;
const SYS_SENDMSG: u64 = 46;
const SYS_RECVMSG: u64 = 47;
const SYS_BIND: u64 = 49;
const SYS_PIPE: u64 = 22;
const SYS_SIGACTION: u64 = 13;
const SYS_SIGPROCMASK: u64 = 14;
const SYS_FORK: u64 = 57;
const SYS_KILL: u64 = 62;

pub const SIGKILL: u32 = 9;
pub const SIGUSR1: u32 = 10;
pub const SIGSEGV: u32 = 11;
pub const SIGUSR2: u32 = 12;
pub const SIGPIPE: u32 = 13;
pub const SIGTERM: u32 = 15;
pub const SIGCHLD: u32 = 17;

pub const SIG_BLOCK: i32 = 0;
pub const SIG_UNBLOCK: i32 = 1;
pub const SIG_SETMASK: i32 = 2;
const SYS_EXEC: u64 = 59;
const SYS_EXIT: u64 = 60;
const SYS_WAITPID: u64 = 61;
#[allow(dead_code)]
const SYS_EPOLL_CREATE: u64 = 213;
const SYS_GETDENTS64: u64 = 217;
const SYS_CLOCK_GETTIME: u64 = 228;
#[allow(dead_code)]
const SYS_EPOLL_WAIT: u64 = 232;
#[allow(dead_code)]
const SYS_EPOLL_CTL: u64 = 233;
const SYS_UPTIME_MS: u64 = 400;
const SYS_UPTIME_SEC: u64 = 401;
const SYS_MEMINFO_KB: u64 = 402;

// ── File I/O ─────────────────────────────────────────────────────────────

pub fn read(fd: i32, buf: &mut [u8]) -> i64 {
    unsafe { syscall3(SYS_READ, fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64) }
}

pub fn write(fd: i32, buf: &[u8]) -> i64 {
    unsafe { syscall3(SYS_WRITE, fd as u64, buf.as_ptr() as u64, buf.len() as u64) }
}

pub fn write_str(fd: i32, s: &str) -> i64 {
    write(fd, s.as_bytes())
}

/// Opens a path (null-terminated required by the kernel). `path` must already
/// include a trailing NUL; use [`with_cstr`] to build one from a `&str`.
pub fn open(path_cstr: &[u8], flags: i32) -> i64 {
    unsafe { syscall2(SYS_OPEN, path_cstr.as_ptr() as u64, flags as u64) }
}

// ── open() flags (must match kernel/src/fs/types.rs::OpenFlags) ────────────

pub const O_RDONLY: i32 = 0;
pub const O_WRONLY: i32 = 1;
#[allow(dead_code)]
pub const O_RDWR: i32 = 2;
pub const O_CREAT: i32 = 0o100;
pub const O_TRUNC: i32 = 0o1000;
#[allow(dead_code)]
pub const O_APPEND: i32 = 0o2000;

pub fn close(fd: i32) -> i64 {
    unsafe { syscall1(SYS_CLOSE, fd as u64) }
}

/// Returns `(read_fd, write_fd)` on success, or the negative errno.
pub fn pipe() -> Result<(i32, i32), i64> {
    let mut fds: [i32; 2] = [0, 0];
    let r = unsafe { syscall1(SYS_PIPE, fds.as_mut_ptr() as u64) };
    if r < 0 { Err(r) } else { Ok((fds[0], fds[1])) }
}

/// `struct stat` — Linux x86-64 ABI layout (144 bytes), matches
/// `kernel/src/fs/types.rs::Stat` exactly.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Stat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_nlink: u64,
    pub st_mode: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    _pad0: u32,
    pub st_rdev: u64,
    pub st_size: i64,
    pub st_blksize: i64,
    pub st_blocks: i64,
    pub st_atime: u64,
    pub st_atime_nsec: u64,
    pub st_mtime: u64,
    pub st_mtime_nsec: u64,
    pub st_ctime: u64,
    pub st_ctime_nsec: u64,
    _reserved: [i64; 3],
}

pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFCHR: u32 = 0o020000;

pub fn stat(path_cstr: &[u8]) -> Result<Stat, i64> {
    let mut st = Stat::default();
    let r = unsafe {
        syscall2(SYS_STAT, path_cstr.as_ptr() as u64, &mut st as *mut Stat as u64)
    };
    if r < 0 { Err(r) } else { Ok(st) }
}

pub fn fstat(fd: i32) -> Result<Stat, i64> {
    let mut st = Stat::default();
    let r = unsafe {
        syscall2(SYS_FSTAT, fd as u64, &mut st as *mut Stat as u64)
    };
    if r < 0 { Err(r) } else { Ok(st) }
}

/// `linux_dirent64`-compatible getdents64. Returns bytes written into `buf`.
pub fn getdents64(fd: i32, buf: &mut [u8]) -> i64 {
    unsafe { syscall3(SYS_GETDENTS64, fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64) }
}

/// One parsed entry from a getdents64 buffer.
pub struct DirentView<'a> {
    pub ino: u64,
    pub d_type: u8,
    pub name: &'a [u8],
    pub record_len: usize,
}

/// Parse a single `linux_dirent64` record at `buf[0..]`. Returns None if buf
/// is too short.
pub fn parse_dirent(buf: &[u8]) -> Option<DirentView<'_>> {
    if buf.len() < 19 {
        return None;
    }
    let ino = u64::from_le_bytes(buf[0..8].try_into().ok()?);
    let reclen = u16::from_le_bytes(buf[16..18].try_into().ok()?) as usize;
    let d_type = buf[18];
    if reclen < 19 || reclen > buf.len() {
        return None;
    }
    // name is NUL-terminated starting at offset 19
    let name_end = buf[19..reclen].iter().position(|&b| b == 0).map(|p| 19 + p).unwrap_or(reclen);
    Some(DirentView { ino, d_type, name: &buf[19..name_end], record_len: reclen })
}

// ── Process control ─────────────────────────────────────────────────────

pub fn exit(status: i32) -> ! {
    unsafe {
        syscall1(SYS_EXIT, status as i64 as u64);
    }
    loop {
        unsafe { asm!("hlt", options(nomem, nostack)); }
    }
}

pub fn getpid() -> i64 {
    unsafe { syscall0(SYS_GETPID) }
}

pub fn yield_now() -> i64 {
    unsafe { syscall0(SYS_YIELD) }
}

/// Returns 0 in the child, > 0 (child pid) in the parent, < 0 on error.
pub fn fork() -> i64 {
    unsafe { syscall0(SYS_FORK) }
}

/// Replaces the current image with the named embedded/initramfs program.
/// Only the path/name is passed — no argv/envp support.
pub fn exec(name_cstr: &[u8]) -> i64 {
    unsafe { syscall1(SYS_EXEC, name_cstr.as_ptr() as u64) }
}

pub fn waitpid(child_pid: i64) -> i64 {
    unsafe { syscall1(SYS_WAITPID, child_pid as u64) }
}

/// Sends `sig` to `pid`. Only single-pid targets (no process groups).
pub fn kill(pid: i64, sig: u32) -> i64 {
    unsafe { syscall2(SYS_KILL, pid as u64, sig as u64) }
}

/// Installs `handler` (an `extern "C" fn(i32)`, cast to a function-pointer
/// bit pattern) for `sig`. Pass `0` for the default action or `1` to
/// ignore. Simplified ABI: the kernel reads/writes a single `u64` handler
/// address, not the full `struct sigaction` (see `kernel/src/process/
/// syscall.rs::sys_sigaction`'s doc comment) — hence the pointer-to-local
/// indirection here.
pub fn sigaction(sig: u32, handler: u64) -> i64 {
    let act: u64 = handler;
    unsafe { syscall3(SYS_SIGACTION, sig as u64, &act as *const u64 as u64, 0) }
}

/// `how` is one of `SIG_BLOCK`/`SIG_UNBLOCK`/`SIG_SETMASK`; `mask` is a
/// bitmask (bit N = signal N). Returns the previous mask via `old_mask`.
pub fn sigprocmask(how: i32, mask: u64, old_mask: Option<&mut u64>) -> i64 {
    let set: u64 = mask;
    let old_ptr = match old_mask {
        Some(r) => r as *mut u64 as u64,
        None => 0,
    };
    unsafe { syscall3(SYS_SIGPROCMASK, how as u64, &set as *const u64 as u64, old_ptr) }
}

// ── Time ─────────────────────────────────────────────────────────────────

pub fn nanosleep(ns: u64) -> i64 {
    unsafe { syscall1(SYS_NANOSLEEP, ns) }
}

pub fn sleep_ms(ms: u64) -> i64 {
    nanosleep(ms * 1_000_000)
}

pub fn uptime_ms() -> i64 {
    unsafe { syscall0(SYS_UPTIME_MS) }
}

pub fn uptime_sec() -> i64 {
    unsafe { syscall0(SYS_UPTIME_SEC) }
}

/// Free physical memory, in KiB.
pub fn meminfo_kb() -> i64 {
    unsafe { syscall0(SYS_MEMINFO_KB) }
}

/// `struct timespec { i64 tv_sec; i64 tv_nsec; }`
pub fn clock_gettime() -> (i64, i64) {
    let mut ts: [i64; 2] = [0, 0];
    unsafe { syscall2(SYS_CLOCK_GETTIME, 0, ts.as_mut_ptr() as u64) };
    (ts[0], ts[1])
}

// ── Memory ───────────────────────────────────────────────────────────────

const MAP_ANONYMOUS: u32 = 0x20;
pub const PROT_READ: u32 = 0x1;
pub const PROT_WRITE: u32 = 0x2;

/// Anonymous-only mmap (matches kernel's sys_mmap restriction: fd must be -1
/// and MAP_ANONYMOUS must be set). Returns the mapped address, or a negative
/// errno.
pub fn mmap_anon(addr_hint: u64, length: u64, prot: u32) -> i64 {
    unsafe {
        syscall5(SYS_MMAP, addr_hint, length, prot as u64, MAP_ANONYMOUS as u64, (-1i64) as u64)
    }
}

pub fn munmap(addr: u64, length: u64) -> i64 {
    unsafe { syscall2(SYS_MUNMAP, addr, length) }
}

// ── IPC (channels) ──────────────────────────────────────────────────────

pub fn socket() -> i64 {
    unsafe { syscall0(SYS_SOCKET) }
}

pub fn bind(fd: i32, path_cstr: &[u8]) -> i64 {
    unsafe { syscall3(SYS_BIND, fd as u64, path_cstr.as_ptr() as u64, path_cstr.len() as u64) }
}

pub fn connect(fd: i32, path_cstr: &[u8]) -> i64 {
    unsafe { syscall3(SYS_CONNECT, fd as u64, path_cstr.as_ptr() as u64, path_cstr.len() as u64) }
}

pub fn accept(fd: i32) -> i64 {
    unsafe { syscall1(SYS_ACCEPT, fd as u64) }
}

/// Wire format for send/recv: `{ tag: u32, len: u32, data: [u8; 56] }` (64 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IpcMsg {
    pub tag: u32,
    pub len: u32,
    pub data: [u8; 56],
}

impl IpcMsg {
    pub fn new(tag: u32, payload: &[u8]) -> Self {
        let mut data = [0u8; 56];
        let n = payload.len().min(56);
        data[..n].copy_from_slice(&payload[..n]);
        Self { tag, len: n as u32, data }
    }
}

pub fn sendmsg(fd: i32, msg: &IpcMsg) -> i64 {
    unsafe { syscall3(SYS_SENDMSG, fd as u64, msg as *const IpcMsg as u64, 0) }
}

pub fn recvmsg(fd: i32, msg: &mut IpcMsg) -> i64 {
    unsafe { syscall3(SYS_RECVMSG, fd as u64, msg as *mut IpcMsg as u64, 0) }
}

// ── poll ─────────────────────────────────────────────────────────────────

pub const POLLIN: i16 = 0x0001;
pub const POLLOUT: i16 = 0x0004;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PollFd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}

pub fn poll(fds: &mut [PollFd], timeout_ms: i32) -> i64 {
    unsafe {
        syscall3(SYS_POLL, fds.as_mut_ptr() as u64, fds.len() as u64, timeout_ms as i64 as u64)
    }
}

// ── C-string helper (no alloc) ──────────────────────────────────────────

/// Builds a NUL-terminated path in a fixed 64-byte stack buffer and calls
/// `f` with the resulting byte slice (including the trailing NUL).
/// Truncates paths longer than 63 bytes.
pub fn with_cstr<R>(s: &str, f: impl FnOnce(&[u8]) -> R) -> R {
    let mut buf = [0u8; 64];
    let n = s.len().min(63);
    buf[..n].copy_from_slice(&s.as_bytes()[..n]);
    f(&buf[..=n])
}
