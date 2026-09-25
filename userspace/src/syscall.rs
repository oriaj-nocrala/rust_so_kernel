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

#[inline(always)]
unsafe fn syscall6(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) -> i64 {
    let ret: i64;
    asm!("syscall", inlateout("rax") nr as i64 => ret,
        in("rdi") a1, in("rsi") a2, in("rdx") a3, in("r10") a4, in("r8") a5, in("r9") a6,
        out("rcx") _, out("r11") _, options(nostack));
    ret
}

// ── Syscall numbers (must match kernel/src/process/syscall.rs::SyscallNumber) ──

const SYS_READ: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_OPEN: u64 = 2;
const SYS_CLOSE: u64 = 3;
const SYS_DUP: u64 = 32;
const SYS_DUP2: u64 = 33;
const SYS_STAT: u64 = 4;
const SYS_FSTAT: u64 = 5;
#[allow(dead_code)]
const SYS_POLL: u64 = 7;
#[allow(dead_code)]
const SYS_LSEEK: u64 = 8;
const SYS_MMAP: u64 = 9;
const SYS_MUNMAP: u64 = 11;
#[allow(dead_code)]
const SYS_YIELD: u64 = 24;
const SYS_NANOSLEEP: u64 = 35;
const SYS_GETPID: u64 = 39;
const SYS_SOCKET: u64 = 41;
const SYS_CONNECT: u64 = 42;
const SYS_ACCEPT: u64 = 43;
const SYS_SENDTO: u64 = 44;
const SYS_RECVFROM: u64 = 45;
const SYS_SENDMSG: u64 = 46;
const SYS_RECVMSG: u64 = 47;
const SYS_SHUTDOWN: u64 = 48;
const SYS_BIND: u64 = 49;
const SYS_LISTEN: u64 = 50;
const SYS_GETSOCKNAME: u64 = 51;
const SYS_GETPEERNAME: u64 = 52;
const SYS_SOCKETPAIR: u64 = 53;
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
const SYS_EPOLL_CREATE: u64 = 213;
const SYS_GETDENTS64: u64 = 217;
const SYS_CLOCK_GETTIME: u64 = 228;
const SYS_EPOLL_WAIT: u64 = 232;
const SYS_EPOLL_CTL: u64 = 233;
const SYS_IOCTL: u64 = 16;
const SYS_FTRUNCATE: u64 = 77;
const SYS_MEMFD_CREATE: u64 = 319;
const SYS_UPTIME_MS: u64 = 400;
const SYS_UPTIME_SEC: u64 = 401;
const SYS_MEMINFO_KB: u64 = 402;
const SYS_KDEBUG_CTL: u64 = 403;
const SYS_REBOOT: u64 = 169;
const SYS_SYNC: u64 = 162;
const SYS_MKDIR: u64 = 83;
const SYS_UNLINK: u64 = 87;
const SYS_SYMLINK: u64 = 88;

/// `target` is stored verbatim, unresolved (real `symlink(2)` semantics).
pub fn symlink(target_cstr: &[u8], linkpath_cstr: &[u8]) -> i64 {
    unsafe { syscall2(SYS_SYMLINK, target_cstr.as_ptr() as u64, linkpath_cstr.as_ptr() as u64) }
}

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

pub fn unlink(path_cstr: &[u8]) -> i64 {
    unsafe { syscall1(SYS_UNLINK, path_cstr.as_ptr() as u64) }
}

pub fn mkdir(path_cstr: &[u8]) -> i64 {
    unsafe { syscall1(SYS_MKDIR, path_cstr.as_ptr() as u64) }
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

/// Duplicates `fd` onto the first free descriptor. Returns the new fd, or
/// a negative errno.
pub fn dup(fd: i32) -> i64 {
    unsafe { syscall1(SYS_DUP, fd as u64) }
}

/// Duplicates `oldfd` onto exactly `newfd` (closing whatever `newfd` was
/// already open on first) — the primitive shell redirection is built on:
/// `cmd > file` is "open file, dup2(fd, 1), close(fd), exec cmd".
pub fn dup2(oldfd: i32, newfd: i32) -> i64 {
    unsafe { syscall2(SYS_DUP2, oldfd as u64, newfd as u64) }
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

/// Replaces the current image with the named embedded/initramfs program,
/// passing an empty argv/envp (argc=0) — a thin wrapper around
/// [`exec_argv`] for callers that don't need to pass arguments.
pub fn exec(name_cstr: &[u8]) -> i64 {
    exec_argv(name_cstr, &[], &[])
}

/// Max argv/envp entries forwarded — matches the kernel's own
/// `MAX_EXEC_ARGS` cap (`kernel/src/process/syscall.rs`), comfortably
/// enough for shell-typed command lines without needing a heap allocation.
const MAX_EXEC_ARGV: usize = 16;

/// Replaces the current image with `path`, passing `args`/`envp` as the
/// new process's argv/envp.
///
/// Every entry must already be NUL-terminated (see [`with_cstr`]) — the
/// kernel reads them straight out of *this* process's memory before the
/// address space is replaced (`kernel/src/process/syscall.rs::sys_exec`),
/// so the pointers only need to stay valid until the syscall returns —
/// which, on success, is never (this process's image is gone).
pub fn exec_argv(path_cstr: &[u8], args: &[&[u8]], envp: &[&[u8]]) -> i64 {
    let mut argv_ptrs = [core::ptr::null::<u8>(); MAX_EXEC_ARGV + 1];
    for (i, a) in args.iter().take(MAX_EXEC_ARGV).enumerate() {
        argv_ptrs[i] = a.as_ptr();
    }

    let mut envp_ptrs = [core::ptr::null::<u8>(); MAX_EXEC_ARGV + 1];
    for (i, e) in envp.iter().take(MAX_EXEC_ARGV).enumerate() {
        envp_ptrs[i] = e.as_ptr();
    }

    unsafe {
        syscall3(
            SYS_EXEC,
            path_cstr.as_ptr() as u64,
            argv_ptrs.as_ptr() as u64,
            envp_ptrs.as_ptr() as u64,
        )
    }
}

pub fn waitpid(child_pid: i64) -> i64 {
    // Must be syscall3, not syscall1: sys_waitpid reads status_ptr/options
    // from rsi/rdx regardless of how many args this wrapper "intends" to
    // pass. syscall1's asm leaves those registers unconstrained, so a
    // syscall1 call here previously handed the kernel whatever garbage was
    // sitting in rsi at the call site as a real user pointer, which the
    // kernel later wrote through — crashing on Rust's alignment/non-null
    // UB check the moment that garbage wasn't a valid aligned address.
    unsafe { syscall3(SYS_WAITPID, child_pid as u64, 0, 0) }
}

/// `waitpid` that also returns the child's raw wait status — this kernel's
/// encoding, not Linux's: `0x200 | code` for an exit, `0x400 | sig << 24`
/// for a kill (`Process::wait_status_word`, mlibc-port's `abi-bits/wait.h`).
/// Returns `(waitpid's return value, status)`.
pub fn waitpid_status(child_pid: i64) -> (i64, i32) {
    let mut status: i32 = 0;
    let r = unsafe { syscall3(SYS_WAITPID, child_pid as u64, &mut status as *mut i32 as u64, 0) };
    (r, status)
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
/// Linux `sigset_t` (bit N-1 = signal N). Returns the previous mask via
/// `old_mask`, same layout.
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

/// Get the current kernel tracing mask (see `kernel::debug`).
pub fn kdebug_get_mask() -> i64 {
    // Explicit 0, 0 for the unused args — never leave stale registers for
    // the kernel to misread as a pointer/flag (see waitpid()'s past bug of
    // exactly this shape).
    unsafe { syscall3(SYS_KDEBUG_CTL, 0, 0, 0) }
}

/// Enable/disable a tracing subsystem by name (e.g. "mm", "sched", "fs",
/// "proc") — `name_cstr` must be NUL-terminated (see [`with_cstr`]).
/// Returns the new mask, or a negative errno if the name is unknown.
pub fn kdebug_set(name_cstr: &[u8], enable: bool) -> i64 {
    unsafe { syscall3(SYS_KDEBUG_CTL, 1, name_cstr.as_ptr() as u64, enable as u64) }
}

/// `struct timespec { i64 tv_sec; i64 tv_nsec; }`
pub fn clock_gettime() -> (i64, i64) {
    let mut ts: [i64; 2] = [0, 0];
    unsafe { syscall2(SYS_CLOCK_GETTIME, 0, ts.as_mut_ptr() as u64) };
    (ts[0], ts[1])
}

// ── Memory ───────────────────────────────────────────────────────────────

pub const PROT_READ: u32 = 0x1;
pub const PROT_WRITE: u32 = 0x2;
pub const MAP_SHARED: u32 = 0x01;
pub const MAP_PRIVATE: u32 = 0x02;
pub const MAP_ANONYMOUS: u32 = 0x20;

/// `mmap(2)` in full (`kernel/src/process/syscall/fs.rs::sys_mmap`): private
/// anonymous memory (`fd == -1`), or `MAP_SHARED` of a memfd / of a fresh
/// shared object (`MAP_SHARED|MAP_ANONYMOUS`). A nonzero `addr` is taken as
/// `MAP_FIXED`. Returns the address, or a negative errno.
pub fn mmap(addr: u64, length: u64, prot: u32, flags: u32, fd: i32, offset: u64) -> i64 {
    unsafe {
        syscall6(SYS_MMAP, addr, length, prot as u64, flags as u64, fd as i64 as u64, offset)
    }
}

/// Private anonymous memory, zero-filled on demand.
pub fn mmap_anon(addr_hint: u64, length: u64, prot: u32) -> i64 {
    mmap(addr_hint, length, prot, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0)
}

pub const MFD_CLOEXEC: u32 = 1;

/// A shared-memory object of size 0 behind a new fd; size it with
/// [`ftruncate`], map it with [`mmap`] + `MAP_SHARED`, pass it with
/// [`send_fds`]. `name_cstr` must be NUL-terminated (display only).
pub fn memfd_create(name_cstr: &[u8], flags: u32) -> i64 {
    unsafe { syscall2(SYS_MEMFD_CREATE, name_cstr.as_ptr() as u64, flags as u64) }
}

/// memfds only (`EINVAL` otherwise). Shrinking a mapped object is `EBUSY`.
pub fn ftruncate(fd: i32, length: u64) -> i64 {
    unsafe { syscall2(SYS_FTRUNCATE, fd as u64, length) }
}

/// `ioctl(fd, request, argp)`; `argp` is whatever the request expects
/// (usually a pointer to its argument struct, cast to `u64`).
pub fn ioctl(fd: i32, request: u64, argp: u64) -> i64 {
    unsafe { syscall3(SYS_IOCTL, fd as u64, request, argp) }
}

pub fn munmap(addr: u64, length: u64) -> i64 {
    unsafe { syscall2(SYS_MUNMAP, addr, length) }
}

// ── AF_UNIX sockets ─────────────────────────────────────────────────────
//
// Real POSIX shapes, not the argument-less `socket()` this used to have:
// the kernel speaks `struct sockaddr_un` (`kernel/src/ipc/unix.rs`, over the
// host-tested `usock` crate), so these wrappers build one.

pub const AF_UNIX: u16 = 1;
pub const SOCK_STREAM: i32 = 1;
pub const SOCK_DGRAM: i32 = 2;

pub const SHUT_RD: i32 = 0;
pub const SHUT_WR: i32 = 1;
pub const SHUT_RDWR: i32 = 2;

/// `struct sockaddr_un { u16 sun_family; char sun_path[108]; }` — the real
/// Linux layout, family field included, two bytes wide.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SockAddrUn {
    pub sun_family: u16,
    pub sun_path: [u8; 108],
}

impl SockAddrUn {
    /// A pathname address. Returns the address and the `addrlen` to pass
    /// alongside it (`sun_path`'s used bytes plus its terminating NUL).
    pub fn path(path: &[u8]) -> (Self, u32) {
        let mut a = Self { sun_family: AF_UNIX, sun_path: [0u8; 108] };
        let n = path.len().min(107);
        a.sun_path[..n].copy_from_slice(&path[..n]);
        (a, (2 + n + 1) as u32)
    }

    /// An abstract-namespace address (`sun_path[0] == '\0'`): no filesystem
    /// node, name visible only to other processes on this kernel.
    pub fn abstract_name(name: &[u8]) -> (Self, u32) {
        let mut a = Self { sun_family: AF_UNIX, sun_path: [0u8; 108] };
        let n = name.len().min(106);
        a.sun_path[1..1 + n].copy_from_slice(&name[..n]);
        (a, (2 + 1 + n) as u32)
    }
}

pub fn socket(domain: i32, ty: i32, protocol: i32) -> i64 {
    unsafe { syscall3(SYS_SOCKET, domain as u64, ty as u64, protocol as u64) }
}

pub fn socketpair(domain: i32, ty: i32, protocol: i32, sv: &mut [i32; 2]) -> i64 {
    unsafe {
        syscall4(SYS_SOCKETPAIR, domain as u64, ty as u64, protocol as u64,
                 sv.as_mut_ptr() as u64)
    }
}

pub fn bind(fd: i32, addr: &SockAddrUn, addrlen: u32) -> i64 {
    unsafe { syscall3(SYS_BIND, fd as u64, addr as *const SockAddrUn as u64, addrlen as u64) }
}

pub fn listen(fd: i32, backlog: i32) -> i64 {
    unsafe { syscall2(SYS_LISTEN, fd as u64, backlog as u64) }
}

pub fn connect(fd: i32, addr: &SockAddrUn, addrlen: u32) -> i64 {
    unsafe { syscall3(SYS_CONNECT, fd as u64, addr as *const SockAddrUn as u64, addrlen as u64) }
}

/// `accept(fd, NULL, NULL)` — the peer address of an AF_UNIX client is
/// almost always unnamed, so callers rarely want it.
pub fn accept(fd: i32) -> i64 {
    unsafe { syscall3(SYS_ACCEPT, fd as u64, 0, 0) }
}

pub fn accept_from(fd: i32, addr: &mut SockAddrUn, addrlen: &mut u32) -> i64 {
    unsafe {
        syscall3(SYS_ACCEPT, fd as u64, addr as *mut SockAddrUn as u64,
                 addrlen as *mut u32 as u64)
    }
}

pub fn send(fd: i32, buf: &[u8]) -> i64 {
    unsafe {
        syscall6(SYS_SENDTO, fd as u64, buf.as_ptr() as u64, buf.len() as u64, 0, 0, 0)
    }
}

pub fn sendto(fd: i32, buf: &[u8], addr: &SockAddrUn, addrlen: u32) -> i64 {
    unsafe {
        syscall6(SYS_SENDTO, fd as u64, buf.as_ptr() as u64, buf.len() as u64, 0,
                 addr as *const SockAddrUn as u64, addrlen as u64)
    }
}

pub fn recv(fd: i32, buf: &mut [u8]) -> i64 {
    unsafe {
        syscall6(SYS_RECVFROM, fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0)
    }
}

pub fn recvfrom(fd: i32, buf: &mut [u8], addr: &mut SockAddrUn, addrlen: &mut u32) -> i64 {
    unsafe {
        syscall6(SYS_RECVFROM, fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0,
                 addr as *mut SockAddrUn as u64, addrlen as *mut u32 as u64)
    }
}

pub fn shutdown(fd: i32, how: i32) -> i64 {
    unsafe { syscall2(SYS_SHUTDOWN, fd as u64, how as u64) }
}

pub fn getsockname(fd: i32, addr: &mut SockAddrUn, addrlen: &mut u32) -> i64 {
    unsafe {
        syscall3(SYS_GETSOCKNAME, fd as u64, addr as *mut SockAddrUn as u64,
                 addrlen as *mut u32 as u64)
    }
}

pub fn getpeername(fd: i32, addr: &mut SockAddrUn, addrlen: &mut u32) -> i64 {
    unsafe {
        syscall3(SYS_GETPEERNAME, fd as u64, addr as *mut SockAddrUn as u64,
                 addrlen as *mut u32 as u64)
    }
}

// ── sendmsg / recvmsg with SCM_RIGHTS ─────────────────────────────────
//
// Linux x86-64 layouts, which the kernel reads verbatim
// (`kernel/src/process/syscall/ipc.rs::read_msghdr`).

pub const MSG_DONTWAIT: u32 = 0x40;
pub const MSG_TRUNC: i32 = 0x20;
pub const MSG_CTRUNC: i32 = 0x8;
const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
/// Most descriptors [`send_fds`]/[`recv_fds`] carry in one message.
pub const MAX_PASSED_FDS: usize = 8;

#[repr(C)]
pub struct IoVec {
    pub base: *mut u8,
    pub len: usize,
}

#[repr(C)]
pub struct MsgHdr {
    pub name: *mut u8,
    pub namelen: u32,
    pub iov: *mut IoVec,
    pub iovlen: usize,
    pub control: *mut u8,
    pub controllen: usize,
    pub flags: i32,
}

#[repr(C)]
struct CmsgHdr {
    len: usize,
    level: i32,
    ty: i32,
}

const CMSG_HDR: usize = core::mem::size_of::<CmsgHdr>();

/// Control buffer for one `SCM_RIGHTS` message of up to `MAX_PASSED_FDS`,
/// 8-aligned as `CMSG_ALIGN` requires.
#[repr(C, align(8))]
struct CmsgBuf([u8; CMSG_HDR + 4 * MAX_PASSED_FDS]);

pub fn sendmsg(fd: i32, msg: &MsgHdr, flags: u32) -> i64 {
    unsafe { syscall3(SYS_SENDMSG, fd as u64, msg as *const MsgHdr as u64, flags as u64) }
}

pub fn recvmsg(fd: i32, msg: &mut MsgHdr, flags: u32) -> i64 {
    unsafe { syscall3(SYS_RECVMSG, fd as u64, msg as *mut MsgHdr as u64, flags as u64) }
}

/// Sends `data` with `fds` attached (`SCM_RIGHTS`). The receiver gets its
/// own descriptors for the same files; ours stay open. `data` must not be
/// empty on a stream socket (nothing would carry the descriptors), and at
/// most [`MAX_PASSED_FDS`] go at once (`EINVAL` beyond).
pub fn send_fds(fd: i32, data: &[u8], fds: &[i32], flags: u32) -> i64 {
    if fds.len() > MAX_PASSED_FDS {
        return -22; // EINVAL
    }
    let mut iov = IoVec { base: data.as_ptr() as *mut u8, len: data.len() };
    let mut cbuf = CmsgBuf([0; CMSG_HDR + 4 * MAX_PASSED_FDS]);
    let clen = CMSG_HDR + 4 * fds.len();
    let hdr = CmsgHdr { len: clen, level: SOL_SOCKET, ty: SCM_RIGHTS };
    unsafe {
        core::ptr::write(cbuf.0.as_mut_ptr() as *mut CmsgHdr, hdr);
        for (i, f) in fds.iter().enumerate() {
            core::ptr::write_unaligned(cbuf.0.as_mut_ptr().add(CMSG_HDR + 4 * i) as *mut i32, *f);
        }
    }
    let msg = MsgHdr {
        name: core::ptr::null_mut(),
        namelen: 0,
        iov: &mut iov,
        iovlen: 1,
        control: if fds.is_empty() { core::ptr::null_mut() } else { cbuf.0.as_mut_ptr() },
        controllen: if fds.is_empty() { 0 } else { clen },
        flags: 0,
    };
    sendmsg(fd, &msg, flags)
}

/// What [`recv_fds`] got: bytes of data, descriptors installed into `fds`,
/// and the `MSG_*` flags the kernel set (`MSG_TRUNC`, `MSG_CTRUNC`).
pub struct Received {
    pub len: usize,
    pub nfds: usize,
    pub flags: i32,
}

/// Receives into `buf`, installing any passed descriptors into `fds` (they
/// are new fds of this process, to close when done). Descriptors that do
/// not fit in `fds` are closed and `MSG_CTRUNC` is set, as on Linux.
/// Returns the negative errno on failure; 0 bytes with 0 fds is EOF.
pub fn recv_fds(fd: i32, buf: &mut [u8], fds: &mut [i32], flags: u32) -> Result<Received, i64> {
    let room = fds.len().min(MAX_PASSED_FDS);
    let mut iov = IoVec { base: buf.as_mut_ptr(), len: buf.len() };
    let mut cbuf = CmsgBuf([0; CMSG_HDR + 4 * MAX_PASSED_FDS]);
    let mut msg = MsgHdr {
        name: core::ptr::null_mut(),
        namelen: 0,
        iov: &mut iov,
        iovlen: 1,
        control: cbuf.0.as_mut_ptr(),
        controllen: CMSG_HDR + 4 * room,
        flags: 0,
    };
    let n = recvmsg(fd, &mut msg, flags);
    if n < 0 {
        return Err(n);
    }
    let mut nfds = 0;
    if msg.controllen >= CMSG_HDR {
        let hdr = unsafe { core::ptr::read(cbuf.0.as_ptr() as *const CmsgHdr) };
        if hdr.level == SOL_SOCKET && hdr.ty == SCM_RIGHTS && hdr.len >= CMSG_HDR {
            nfds = ((hdr.len - CMSG_HDR) / 4).min(room);
            for (i, slot) in fds.iter_mut().take(nfds).enumerate() {
                *slot = unsafe {
                    core::ptr::read_unaligned(cbuf.0.as_ptr().add(CMSG_HDR + 4 * i) as *const i32)
                };
            }
        }
    }
    Ok(Received { len: n as usize, nfds, flags: msg.flags })
}

// ── epoll ────────────────────────────────────────────────────────────────

pub const EPOLLIN: u32 = 0x001;
pub const EPOLLOUT: u32 = 0x004;
pub const EPOLLERR: u32 = 0x008;
pub const EPOLLHUP: u32 = 0x010;
pub const EPOLLET: u32 = 0x8000_0000;
pub const EPOLL_CTL_ADD: i32 = 1;
pub const EPOLL_CTL_DEL: i32 = 2;
pub const EPOLL_CTL_MOD: i32 = 3;
/// `sys_epoll_wait` refuses more than this per call.
pub const EPOLL_MAX_EVENTS: usize = 16;

/// `struct epoll_event`: packed on x86-64 Linux (12 bytes), and so here.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct EpollEvent {
    pub events: u32,
    pub data: u64,
}

pub fn epoll_create() -> i64 {
    unsafe { syscall1(SYS_EPOLL_CREATE, 1) }
}

/// `data` comes back unchanged in every event for `fd`.
pub fn epoll_ctl(epfd: i32, op: i32, fd: i32, events: u32, data: u64) -> i64 {
    let ev = EpollEvent { events, data };
    unsafe {
        syscall4(SYS_EPOLL_CTL, epfd as u64, op as u64, fd as u64, &ev as *const EpollEvent as u64)
    }
}

/// Fills at most `min(events.len(), EPOLL_MAX_EVENTS)` entries. `-1` waits
/// forever, `0` does not wait.
pub fn epoll_wait(epfd: i32, events: &mut [EpollEvent], timeout_ms: i32) -> i64 {
    let max = events.len().min(EPOLL_MAX_EVENTS);
    unsafe {
        syscall4(SYS_EPOLL_WAIT, epfd as u64, events.as_mut_ptr() as u64, max as u64,
                 timeout_ms as i64 as u64)
    }
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

// ── Power ────────────────────────────────────────────────────────────────

/// `sync(2)`: here, copies the kernel log ring to the USB stick's log
/// partition (ext2 writes are already synchronous). Returns 0 or -errno.
pub fn sync() -> i64 {
    unsafe { syscall0(SYS_SYNC) }
}

/// `reboot(LINUX_REBOOT_CMD_RESTART)` with Linux's magic numbers. Only
/// returns on failure (negative errno).
pub fn reboot() -> i64 {
    const MAGIC1: u64 = 0xfee1_dead;
    const MAGIC2: u64 = 672_274_793;
    const CMD_RESTART: u64 = 0x0123_4567;
    unsafe { syscall3(SYS_REBOOT, MAGIC1, MAGIC2, CMD_RESTART) }
}
