// kernel/src/process/syscall/fs.rs
//
// File/fd/path syscalls: read/write/open/close/stat family/getdents64/
// lseek/mmap/munmap/pipe/dup/dup2/fcntl/ioctl/writev/access/rename/mkdir/
// rmdir/unlink/symlink/readlink/chmod/fchmod/statvfs/getcwd/chdir, plus the
// stdin blocking-read machinery (keyboard ISR wakeup path).

use crate::sync::Mutex;
use crate::process::TrapFrame;
use super::{
    errno, SyscallResult, with_current_process, with_files, with_fd_table, validate_user_buffer,
    resolve_path, current_cwd, read_user_str, current_tf_ptr, note_denied,
};
use vfs::rights::{self, Rights};

struct StdinWaiter {
    pid: usize,
    user_buf: u64,
    /// The wait's cell (`process::wait`): claimed by `stdin_wakeup` before
    /// it consumes a key for this reader.
    cell: alloc::sync::Arc<crate::process::wait::WaitCell>,
}

/// The interruptible wait of a `read`/`write` that blocks in a
/// `FileHandle` (a pipe, a socket, the console): restarted under
/// `SA_RESTART`, EINTR otherwise. The syscall number is read from the frame
/// rather than assumed — `writev` blocks through `sys_write` — and
/// `entry_rip` is taken before the handle runs, since a socket rewinds the
/// frame's `rip` on its way to blocking.
fn io_wait(tf: *const TrapFrame, entry_rip: u64) -> crate::process::wait::Wait {
    crate::process::wait::Wait::cell(
        unsafe { (*tf).rax },
        entry_rip,
        crate::process::wait::RestartPolicy::SaRestart,
        crate::process::wait::Cleanup::None,
    )
}

static STDIN_WAITER: crate::sync::IrqLock<Option<StdinWaiter>> = crate::sync::IrqLock::new(None);

// ============================================================================
// SYSCALL IMPLEMENTATIONS
// ============================================================================

/// True iff fd 0 is still bound to the real console device.
///
/// `sys_read`'s fd==0 fast path bypasses the file table entirely and reads
/// straight from the keyboard ISR buffer, blocking the caller until a key
/// arrives — required for an interactive shell, since `SerialConsole::read`
/// itself is non-blocking (see its doc comment). But that fast path used to
/// fire unconditionally, so once fd 0 had been `dup2`'d onto a file or pipe
/// (e.g. `tr a-z A-Z < file`), reads still went to the keyboard buffer
/// instead of the redirected file — the redirect was silently ignored and
/// the program blocked on real keyboard input forever. This gate restricts
/// the keyboard fast path to fd 0 handles that are still the console;
/// anything else (a redirected file, a pipe) falls through to the generic
/// file-table path below, same as any other fd.
fn stdin_is_console() -> bool {
    with_fd_table(|t| t.get(0).map(|h| h.name() == "serial").unwrap_or(false)).unwrap_or(false)
}

pub(super) fn sys_read(fd: i32, buf: usize, count: usize) -> SyscallResult {
    if count == 0 {
        return 0;
    }
    if let Err(e) = validate_user_buffer(buf as u64, count) {
        return e;
    }

    let stdin_console = fd == 0 && stdin_is_console();
    crate::ktrace!(crate::debug::FS, "sys_read: fd={} count={} stdin_console={}", fd, count, stdin_console);
    if stdin_console {
        // stdin, still bound to the real console (not redirected via
        // dup2): read from the keyboard buffer; block if empty.
        //
        // The guard prevents a race between the buffer-empty check and
        // setting STDIN_WAITER — the keyboard ISR could fire between them
        // otherwise. On the blocking path below, `irq` is deliberately never
        // dropped: `block_stdin_read` diverges (never returns to this stack
        // frame), so its `Drop`/`sti` glue simply never runs — interrupts
        // stay off across the jump, same as before this was RAII.
        let irq = crate::process::irq_guard::InterruptGuard::new();

        if let Some(c) = crate::keyboard::read_key() {
            drop(irq);
            // Process's page table is active — write directly to user VA.
            unsafe { *(buf as *mut u8) = c as u8; }
            return 1;
        }

        // Buffer empty — register waiter and block.
        let tf_ptr = current_tf_ptr();
        block_stdin_read(tf_ptr, buf as u64)
    } else {
        // Continuous cli from before the fd lookup through either the fast
        // return or the block — same shape as sys_futex's FUTEX_WAIT. This
        // matters because a pipe's `read()` may return WouldBlock, at which
        // point this function does the actual block_current/jump_to_trapframe
        // itself; that must never happen while SCHEDULER or the fd table are
        // still held (SCHEDULER: self-deadlock, spin::Mutex isn't reentrant;
        // fd table: jump_to_trapframe diverges, so a guard alive across it
        // would never run its Drop and stays locked forever — see sys_close's
        // doc comment for the same class of hazard). `irq`'s Drop handles
        // every early-return path below automatically; on the WouldBlock
        // path it's deliberately left undropped, same reasoning as above.
        let _irq = crate::process::irq_guard::InterruptGuard::new();
        let entry_rip = unsafe { (*current_tf_ptr()).rip };

        let files = {
            let scheduler = crate::process::scheduler::local_scheduler();
            match scheduler.running_ref() {
                Some(proc) => proc.files.clone(),
                None => return errno::ESRCH,
            }
        };

        let result = {
            let mut files_guard = files.lock();
            match files_guard.get_mut(fd as usize) {
                Ok(file) => {
                    let buffer = unsafe {
                        core::slice::from_raw_parts_mut(buf as *mut u8, count)
                    };
                    file.read(buffer)
                }
                Err(_) => return errno::EBADF,
            }
        };

        match result {
            Ok(n) => n as i64,
            Err(crate::process::file::FileError::Again) => errno::EAGAIN,
            Err(crate::process::file::FileError::WouldBlock) => {
                // `jump_to_user` never returns, so nothing on this stack
                // frame is ever dropped: release the fd-table `Arc` first.
                // Leaking it kept the table — and every pipe end in it —
                // alive after the process exited, so a reader of that
                // pipe never saw EOF (`seq 1 1100 | wc -c` hung whenever
                // `seq` had blocked on a full pipe).
                drop(files);
                let tf_ptr = current_tf_ptr();
                let next_tf = {
                    let mut scheduler = crate::process::scheduler::local_scheduler();
                    scheduler.block_current(tf_ptr, io_wait(tf_ptr, entry_rip))
                };
                unsafe { crate::process::trapframe::jump_to_user(next_tf) }
            }
            Err(crate::process::file::FileError::InvalidInput) => errno::EINVAL,
            Err(crate::process::file::FileError::NotConnected) => errno::ENOTCONN,
            Err(crate::process::file::FileError::ConnectionReset) => errno::ECONNRESET,
            Err(_) => errno::EIO,
        }
    }
}

/// Block the calling process waiting for keyboard input.
///
/// cli must already be in effect when this is called.
/// Saves the current TrapFrame into the process Box, moves the process to the
/// wait_queue, and jumps to the next Ready process.  Never returns.
///
/// Registering in `STDIN_WAITER` and blocking are one step under the
/// scheduler lock (stage 7 of docs/smp/smp-plan.md): `stdin_wakeup` takes
/// `STDIN_WAITER` and then the scheduler lock, so it never finds this
/// process between the two, running on another CPU. And the keyboard buffer
/// is checked again with `STDIN_WAITER` held: the ISR pushes a key *before*
/// it takes `STDIN_WAITER`, so a key that arrived after the caller's check
/// is either seen here or finds this waiter — the syscall is re-executed in
/// the first case (`rip -= 2`, `rax` still its number).
fn block_stdin_read(current_tf: *const TrapFrame, user_buf: u64) -> ! {
    let next_tf = {
        let mut sched = crate::process::scheduler::local_scheduler();
        let pid = sched.current_pid().map(|p| p.0).unwrap_or(0);
        let mut waiter = STDIN_WAITER.lock();
        if crate::keyboard::read_key_peek() {
            None
        } else {
            let cell = sched.begin_wait();
            *waiter = Some(StdinWaiter { pid, user_buf, cell });
            drop(waiter);
            let entry_rip = unsafe { (*current_tf).rip };
            Some(sched.block_current(current_tf, io_wait(current_tf, entry_rip)))
        }
        // Lock dropped here; sti happens via iretq of the next process.
    };
    match next_tf {
        Some(tf) => unsafe { crate::process::trapframe::jump_to_user(tf) },
        None => unsafe {
            (*(current_tf as *mut TrapFrame)).rip -= 2;
            crate::process::trapframe::jump_to_user(current_tf)
        },
    }
}

/// Called by the keyboard ISR after a key is pushed into the buffer.
///
/// If a process is blocked on stdin, delivers the character to its user
/// buffer (through its address space), sets rax=1 in its saved
/// TrapFrame, and moves it back to the run queue.
/// Deliver `sig` to every process in group `pgid` — used by the tty line
/// discipline (Ctrl-C/Ctrl-Z, see `tty::feed_input`) from ISR context,
/// where interrupts are already off, so (like `stdin_wakeup` below) this
/// locks the scheduler directly with no explicit cli/sti.
pub(crate) fn send_to_group(pgid: u32, sig: u32) {
    crate::process::scheduler::local_scheduler().queue_signal_to_group(pgid, sig);
}

pub(crate) fn stdin_wakeup() {
    // Take the waiter atomically — if no one is waiting, return immediately.
    let waiter = {
        let mut w = STDIN_WAITER.lock();
        w.take()
    };
    let Some(waiter) = waiter else { return; };

    if !crate::keyboard::read_key_peek() {
        // Shouldn't happen (the ISR pushed a key just before calling us).
        if waiter.cell.is_armed() {
            *STDIN_WAITER.lock() = Some(waiter);
        }
        return;
    }
    // Before consuming the key: a reader a signal interrupted is gone, and
    // the key belongs to whoever reads next.
    if !waiter.cell.claim() {
        return;
    }
    let key = crate::keyboard::read_key();

    let user_buf = waiter.user_buf;
    let pid = waiter.pid;

    let mut sched = crate::process::scheduler::local_scheduler();

    // Find the blocked process, write the character into its buffer through
    // its own address space (with a user write's semantics — a COW-shared
    // or zero-frame page gets a private copy first, see
    // `AddressSpace::copy_to_user`), and set rax=1 as the return value.
    // Taking the address space's `IrqMutex` from this ISR is safe: every
    // holder runs with IF=0, so none can be the code this interrupted.
    // Without a key after all (another CPU took it between the peek and
    // here), the claimed wait is ended by re-executing the read.
    for proc in sched.wait_queue_mut().iter_mut() {
        if proc.pid.0 == pid && matches!(proc.state, crate::process::ProcessState::Blocked) {
            match key {
                Some(c) if unsafe { proc.address_space.copy_to_user(user_buf, &[c as u8]) } == 1 => {
                    proc.trapframe.rax = 1; // syscall return value: 1 byte read
                }
                _ => proc.trapframe.rip -= 2, // rax still holds read's number
            }
            break;
        }
    }

    sched.wake(pid);
}

/// sys_write — same non-reentrant shape as sys_read's fd>0 branch (see its
/// comment): the fd-table lock must be released before any potential block,
/// since `file.write()` (e.g. a full pipe) may need to register a waiter and
/// return `WouldBlock`, at which point *this* function does the actual
/// cli/block_current/jump_to_trapframe dance itself.
pub(super) fn sys_write(fd: i32, buf: usize, count: usize) -> SyscallResult {
    if let Err(e) = validate_user_buffer(buf as u64, count) {
        return e;
    }
    crate::ktrace!(crate::debug::FS, "sys_write: fd={} count={}", fd, count);

    let _irq = crate::process::irq_guard::InterruptGuard::new();
    let entry_rip = unsafe { (*current_tf_ptr()).rip };

    let files = {
        let scheduler = crate::process::scheduler::local_scheduler();
        match scheduler.running_ref() {
            Some(proc) => proc.files.clone(),
            None => return errno::ESRCH,
        }
    };

    let result = {
        let mut files_guard = files.lock();
        match files_guard.get_mut(fd as usize) {
            Ok(file) => {
                let buffer = unsafe {
                    core::slice::from_raw_parts(buf as *const u8, count)
                };
                file.write(buffer)
            }
            Err(_) => return errno::EBADF,
        }
    };

    match result {
        Ok(n) => n as i64,
        Err(crate::process::file::FileError::Again) => errno::EAGAIN,
        Err(crate::process::file::FileError::BrokenPipe) => errno::EPIPE,
        Err(crate::process::file::FileError::NoSpace) => errno::ENOSPC,
        Err(crate::process::file::FileError::InvalidInput) => errno::EINVAL,
        Err(crate::process::file::FileError::NotConnected) => errno::ENOTCONN,
        Err(crate::process::file::FileError::ConnectionReset) => errno::ECONNRESET,
        Err(crate::process::file::FileError::WouldBlock) => {
            // Same as `sys_read`'s WouldBlock arm: `jump_to_user` never
            // returns, so drop the fd-table `Arc` before it or the table
            // outlives the process (the pipe-EOF hang).
            drop(files);
            let tf_ptr = current_tf_ptr();
            let next_tf = {
                let mut scheduler = crate::process::scheduler::local_scheduler();
                scheduler.block_current(tf_ptr, io_wait(tf_ptr, entry_rip))
            };
            unsafe { crate::process::trapframe::jump_to_user(next_tf) }
        }
        Err(_) => errno::EIO,
    }
}

pub(super) fn sys_open(path_ptr: usize, flags: i32) -> SyscallResult {
    open_at(AT_FDCWD, path_ptr, flags)
}

/// openat(257): `(dirfd, path, flags, mode)`; `mode` is not kept (no permission model), as for `open`.
pub(super) fn sys_openat(dirfd: i64, path_ptr: usize, flags: i32, _mode: u32) -> SyscallResult {
    open_at(dirfd, path_ptr, flags)
}

/// openat2(437): `(dirfd, path, how, size)`. `how` is `struct open_how { flags, mode, resolve }`, validated as Linux does
/// (`vfs::resolve::OpenHow::parse`: `E2BIG`/`EINVAL`). With `resolve == 0` it is `openat`. `RESOLVE_BENEATH` and
/// `RESOLVE_NO_SYMLINKS` go through the bounded walk (`MountTable::resolve_at`), which checks every `..` and symlink
/// against the dirfd's directory (`EXDEV`/`ELOOP`); the other `RESOLVE_*` bits are `EINVAL` for now.
pub(super) fn sys_openat2(dirfd: i64, path_ptr: usize, how_ptr: usize, size: usize) -> SyscallResult {
    if size > vfs::resolve::OPEN_HOW_SIZE_MAX {
        return errno::E2BIG;
    }
    if size < vfs::resolve::OPEN_HOW_SIZE_VER0 {
        return errno::EINVAL;
    }
    if let Err(e) = validate_user_buffer(how_ptr as u64, size) {
        return e;
    }
    let mut how_bytes = alloc::vec![0u8; size];
    unsafe { core::ptr::copy_nonoverlapping(how_ptr as *const u8, how_bytes.as_mut_ptr(), size) };
    let how = match vfs::resolve::OpenHow::parse(&how_bytes) { Ok(h) => h, Err(e) => return e.as_i64() };
    let flags = how.flags as i32;
    if how.resolve == 0 {
        return open_at(dirfd, path_ptr, flags);
    }

    if let Err(e) = validate_user_buffer(path_ptr as u64, 1) {
        return e;
    }
    let raw = read_user_str(path_ptr);
    if raw.is_empty() {
        return errno::ENOENT;
    }
    // Capability mode: only beneath a real dirfd (`vfs::capmode`).
    let mut resolve = how.resolve;
    if crate::process::scheduler::in_capmode() {
        if dirfd == AT_FDCWD || raw.starts_with('/') {
            let from = if raw.starts_with('/') { "/" } else { "the cwd" };
            return super::record_denial(format_args!("openat2: path '{}' from {}: ECAPMODE (capability mode)", raw, from), errno::ECAPMODE);
        }
        resolve |= vfs::resolve::RESOLVE_BENEATH;
    }
    // Through a real dirfd: that fd's rights must allow the open, and the new fd gets them (as `openat`).
    let (base, inherited) = if dirfd == AT_FDCWD || raw.starts_with('/') {
        (current_cwd(), rights::CAP_ALL)
    } else {
        match dir_path_of(dirfd, rights::openat_needs(flags), "openat2") { Ok(p) => p, Err(e) => return e }
    };
    crate::ktrace!(crate::debug::FS, "sys_openat2: base={} path={} flags={:#x} resolve={:#x}", base, raw, flags, how.resolve);
    match crate::fs::vfs::open_at(&base, raw, crate::fs::types::OpenFlags(flags), resolve) {
        Ok((handle, path)) => install_fd(handle, flags, path, inherited),
        Err(e) => e.as_i64(),
    }
}

fn open_at(dirfd: i64, path_ptr: usize, flags: i32) -> SyscallResult {
    // Validation BEFORE cli — no lock needed
    let (path, inherited) = match user_path_at_rights(dirfd, path_ptr, errno::EINVAL, rights::openat_needs(flags), "openat", flags & 0o400000 == 0) {
        Ok(p) => p,
        Err(e) => return e,
    };
    crate::ktrace!(crate::debug::FS, "sys_open: path={} flags={:#x}", path, flags);

    // Resolve through VFS: /dev/* → drivers, /bin/* → initramfs, …
    // Box allocation uses Slab (different lock from SCHEDULER).
    let handle = match crate::fs::vfs::open(&path, crate::fs::types::OpenFlags(flags)) {
        Ok(h)  => h,
        Err(e) => { crate::ktrace!(crate::debug::FS, "sys_open: {} -> Err({:?})", path, e); return e.as_i64(); }
    };
    install_fd(handle, flags, path, inherited)
}

/// Put a freshly opened handle in the calling process's fd table with capability `rights`, honouring `O_NONBLOCK` and
/// `O_CLOEXEC`, and record `path` for later `*at` calls through it.
fn install_fd(handle: alloc::boxed::Box<dyn crate::process::file::FileHandle>, flags: i32, path: alloc::string::String, rights: Rights) -> SyscallResult {
    // `O_NONBLOCK` at open, for the handles that honour it (sockets, ptys);
    // the rest ignore it, as they ignore `F_SETFL` (see `sys_fcntl`).
    if flags as i64 & O_NONBLOCK != 0 {
        handle.set_nonblocking(true);
    }

    // Only take scheduler lock for the FD table insertion
    with_files(|files| {
        match files.allocate_with_rights(handle, rights) {
            Ok(fd) => {
                if flags & O_CLOEXEC != 0 {
                    let _ = files.set_cloexec(fd, true);
                }
                // Kept so a later `openat(fd, "rel")` (this fd as a `dirfd`) can resolve.
                files.set_path(fd, path.clone());
                fd as i64
            }
            Err(_) => errno::EMFILE, // the table is full
        }
    })
}

/// Mark an fd of the calling process close-on-exec (`SOCK_CLOEXEC`, `MFD_CLOEXEC`, …).
pub(super) fn set_cloexec_current(fd: usize) {
    with_files(|files| {
        let _ = files.set_cloexec(fd, true);
        0
    });
}

pub(super) fn sys_stat(path_ptr: usize, stat_ptr: usize) -> SyscallResult {
    stat_impl(AT_FDCWD, path_ptr, stat_ptr, true)
}

/// newfstatat(262): `(dirfd, path, statbuf, flags)`. `AT_SYMLINK_NOFOLLOW` is `lstat`; `AT_EMPTY_PATH` with an empty path is
/// `fstat(dirfd)`.
pub(super) fn sys_newfstatat(dirfd: i64, path_ptr: usize, stat_ptr: usize, flags: u64) -> SyscallResult {
    const AT_EMPTY_PATH: u64 = 0x1000;
    const AT_NO_AUTOMOUNT: u64 = 0x800;
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH | AT_NO_AUTOMOUNT) != 0 {
        return errno::EINVAL;
    }
    if flags & AT_EMPTY_PATH != 0 && validate_user_buffer(path_ptr as u64, 1).is_ok() && read_user_str(path_ptr).is_empty() {
        if let Err(e) = require_fd(dirfd, rights::CAP_FSTAT, "newfstatat") { return e; }
        return sys_fstat(dirfd as i32, stat_ptr);
    }
    stat_impl(dirfd, path_ptr, stat_ptr, flags & AT_SYMLINK_NOFOLLOW == 0)
}

/// lstat(6): like `stat`, but doesn't follow a symlink at the final path
/// component — reports the link itself (`FileType::Symlink`, `st_size` =
/// target length). Used to just alias `sys_stat` outright ("no symlinks
/// yet"); now that `fs::vfs` has real symlink support (see `fs::procfs`),
/// this is the genuine no-follow lookup.
pub(super) fn sys_lstat(path_ptr: usize, stat_ptr: usize) -> SyscallResult {
    stat_impl(AT_FDCWD, path_ptr, stat_ptr, false)
}

/// A filesystem that keeps no times (initramfs, devfs, procfs) reports 0
/// for all three; hand those out as the boot time instead of the epoch —
/// they did come into being at boot — so `ls -l` and `find -newer` see
/// something true-ish rather than 1970. ext2 and ramfs report their own.
fn fill_missing_times(stat: &mut crate::fs::types::Stat) {
    if stat.st_atime == 0 && stat.st_mtime == 0 && stat.st_ctime == 0 {
        let boot = crate::time::boot_unix_secs();
        stat.st_atime = boot;
        stat.st_mtime = boot;
        stat.st_ctime = boot;
    }
}

fn stat_impl(dirfd: i64, path_ptr: usize, stat_ptr: usize, follow: bool) -> SyscallResult {
    use crate::fs::types::Stat;
    if let Err(e) = validate_user_buffer(stat_ptr as u64, core::mem::size_of::<Stat>()) { return e; }

    let path = match user_path_at(dirfd, path_ptr, errno::ENOENT, rights::CAP_FSTAT, "fstatat", follow) { Ok(p) => p, Err(e) => return e };
    let result = if follow { crate::fs::stat(&path) } else { crate::fs::lstat(&path) };
    match result {
        Err(e)   => e.as_i64(),
        Ok(mut stat) => {
            fill_missing_times(&mut stat);
            unsafe { core::ptr::write(stat_ptr as *mut Stat, stat); }
            0
        }
    }
}

/// statx(332): `(dirfd, path, flags, mask, statxbuf)`. The same lookups as `newfstatat`, in the 256-byte `struct statx`. It
/// reports the basic fields (`STATX_BASIC_STATS`) whatever `mask` asks: this kernel keeps no birth time, so `STATX_BTIME` is
/// never in the returned mask, which is how a caller learns it is missing (Rust's `Metadata::created` then says unsupported).
pub(super) fn sys_statx(dirfd: i64, path_ptr: usize, flags: u64, mask: u32, buf: usize) -> SyscallResult {
    use crate::fs::types::Stat;
    const AT_EMPTY_PATH: u64 = 0x1000;
    const AT_NO_AUTOMOUNT: u64 = 0x800;
    const AT_STATX_SYNC_TYPE: u64 = 0x6000;
    const STATX__RESERVED: u32 = 0x8000_0000;
    const STATX_BASIC_STATS: u32 = 0x7ff;
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH | AT_NO_AUTOMOUNT | AT_STATX_SYNC_TYPE) != 0 || mask & STATX__RESERVED != 0 {
        return errno::EINVAL;
    }
    if let Err(e) = validate_user_buffer(buf as u64, 256) { return e; }

    let empty = flags & AT_EMPTY_PATH != 0 && validate_user_buffer(path_ptr as u64, 1).is_ok() && read_user_str(path_ptr).is_empty();
    let result: Result<Stat, i64> = if empty {
        if let Err(e) = require_fd(dirfd, rights::CAP_FSTAT, "statx") { return e; }
        with_fd_table(|t| t.get(dirfd as usize).ok().and_then(|f| f.stat())).flatten().ok_or(errno::EBADF)
    } else {
        match user_path_at(dirfd, path_ptr, errno::ENOENT, rights::CAP_FSTAT, "statx", flags & AT_SYMLINK_NOFOLLOW == 0) {
            Err(e) => Err(e),
            Ok(path) => {
                let r = if flags & AT_SYMLINK_NOFOLLOW == 0 { crate::fs::stat(&path) } else { crate::fs::lstat(&path) };
                r.map_err(|e| e.as_i64())
            }
        }
    };
    let mut st = match result { Ok(st) => st, Err(e) => return e };
    fill_missing_times(&mut st);

    // Linux's `major(dev)`/`minor(dev)` for a 64-bit dev_t.
    let major = |d: u64| (((d >> 8) & 0xfff) | ((d >> 32) & !0xfff)) as u32;
    let minor = |d: u64| ((d & 0xff) | ((d >> 12) & !0xff)) as u32;
    let mut out = [0u8; 256];
    let mut put = |off: usize, bytes: &[u8]| out[off..off + bytes.len()].copy_from_slice(bytes);
    put(0, &STATX_BASIC_STATS.to_ne_bytes());
    put(4, &(st.st_blksize as u32).to_ne_bytes());
    put(16, &(st.st_nlink as u32).to_ne_bytes());
    put(20, &st.st_uid.to_ne_bytes());
    put(24, &st.st_gid.to_ne_bytes());
    put(28, &(st.st_mode as u16).to_ne_bytes());
    put(32, &st.st_ino.to_ne_bytes());
    put(40, &(st.st_size as u64).to_ne_bytes());
    put(48, &(st.st_blocks as u64).to_ne_bytes());
    for (off, sec, nsec) in [(64, st.st_atime, st.st_atime_nsec), (96, st.st_ctime, st.st_ctime_nsec), (112, st.st_mtime, st.st_mtime_nsec)] {
        put(off, &(sec as i64).to_ne_bytes());
        put(off + 8, &(nsec as u32).to_ne_bytes());
    }
    put(128, &major(st.st_rdev).to_ne_bytes());
    put(132, &minor(st.st_rdev).to_ne_bytes());
    put(136, &major(st.st_dev).to_ne_bytes());
    put(140, &minor(st.st_dev).to_ne_bytes());
    unsafe { core::ptr::copy_nonoverlapping(out.as_ptr(), buf as *mut u8, 256); }
    0
}

pub(super) fn sys_fstat(fd: i32, stat_ptr: usize) -> SyscallResult {
    use crate::fs::types::Stat;
    if let Err(e) = validate_user_buffer(stat_ptr as u64, core::mem::size_of::<Stat>()) { return e; }

    // Retrieve stat outside with_current_process to avoid holding the scheduler lock
    // while doing a potentially expensive write.
    let stat_result: Option<Stat> = with_fd_table(|t| t.get(fd as usize).ok().and_then(|f| f.stat())).flatten();

    match stat_result {
        None       => errno::EBADF,
        Some(mut stat) => {
            fill_missing_times(&mut stat);
            unsafe { core::ptr::write(stat_ptr as *mut Stat, stat); }
            0
        }
    }
}

/// mkdir(83): long mkdir(const char *path) — no `mode` param, matching
/// this kernel's `open()` (which also drops the POSIX `mode` argument):
/// nothing here enforces permission bits, so there's nothing to store it
/// in.
pub(super) fn sys_mkdir(path_ptr: usize) -> SyscallResult {
    mkdir_at(AT_FDCWD, path_ptr)
}

/// mkdirat(258): `(dirfd, path, mode)`.
pub(super) fn sys_mkdirat(dirfd: i64, path_ptr: usize, _mode: u32) -> SyscallResult {
    mkdir_at(dirfd, path_ptr)
}

fn mkdir_at(dirfd: i64, path_ptr: usize) -> SyscallResult {
    let path = match user_path_at(dirfd, path_ptr, errno::EINVAL, rights::CAP_MKDIRAT, "mkdirat", false) { Ok(p) => p, Err(e) => return e };
    match crate::fs::vfs::mkdir(&path) {
        Ok(())  => 0,
        Err(e)  => e.as_i64(),
    }
}

/// rmdir(84): long rmdir(const char *path)
pub(super) fn sys_rmdir(path_ptr: usize) -> SyscallResult {
    rmdir_at(AT_FDCWD, path_ptr)
}

fn rmdir_at(dirfd: i64, path_ptr: usize) -> SyscallResult {
    let path = match user_path_at(dirfd, path_ptr, errno::EINVAL, rights::CAP_UNLINKAT, "unlinkat", false) { Ok(p) => p, Err(e) => return e };
    match crate::fs::vfs::rmdir(&path) {
        Ok(())  => 0,
        Err(e)  => e.as_i64(),
    }
}

/// unlink(87): long unlink(const char *path)
pub(super) fn sys_unlink(path_ptr: usize) -> SyscallResult {
    unlink_at(AT_FDCWD, path_ptr)
}

/// unlinkat(263): `(dirfd, path, flags)`; `AT_REMOVEDIR` makes it `rmdir`.
pub(super) fn sys_unlinkat(dirfd: i64, path_ptr: usize, flags: u64) -> SyscallResult {
    const AT_REMOVEDIR: u64 = 0x200;
    if flags & !AT_REMOVEDIR != 0 {
        return errno::EINVAL;
    }
    if flags & AT_REMOVEDIR != 0 { rmdir_at(dirfd, path_ptr) } else { unlink_at(dirfd, path_ptr) }
}

fn unlink_at(dirfd: i64, path_ptr: usize) -> SyscallResult {
    let path = match user_path_at(dirfd, path_ptr, errno::EINVAL, rights::CAP_UNLINKAT, "unlinkat", false) { Ok(p) => p, Err(e) => return e };
    match crate::fs::vfs::unlink(&path) {
        Ok(())  => 0,
        Err(e)  => e.as_i64(),
    }
}

/// readlink(89): long readlink(const char *path, char *buf, size_t bufsiz)
///
/// Returns the number of bytes written into `buf` (never NUL-terminated,
/// matching real `readlink(2)`) — truncated silently to `bufsiz` if the
/// target is longer, same as real POSIX.
pub(super) fn sys_readlink(path_ptr: usize, buf_ptr: usize, bufsiz: usize) -> SyscallResult {
    readlink_at(AT_FDCWD, path_ptr, buf_ptr, bufsiz)
}

/// readlinkat(267): `(dirfd, path, buf, bufsiz)`.
pub(super) fn sys_readlinkat(dirfd: i64, path_ptr: usize, buf_ptr: usize, bufsiz: usize) -> SyscallResult {
    readlink_at(dirfd, path_ptr, buf_ptr, bufsiz)
}

fn readlink_at(dirfd: i64, path_ptr: usize, buf_ptr: usize, bufsiz: usize) -> SyscallResult {
    if let Err(e) = validate_user_buffer(buf_ptr as u64, bufsiz) { return e; }
    let path = match user_path_at(dirfd, path_ptr, errno::EINVAL, 0, "readlinkat", false) { Ok(p) => p, Err(e) => return e };
    match crate::fs::readlink(&path) {
        Ok(target) => {
            let bytes = target.as_bytes();
            let n = bytes.len().min(bufsiz);
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf_ptr as *mut u8, n);
            }
            n as SyscallResult
        }
        Err(e) => e.as_i64(),
    }
}

/// symlink(88): long symlink(const char *target, const char *linkpath)
///
/// `target` is stored verbatim — not resolved, not required to exist
/// (matches real `symlink(2)`: a dangling symlink is legal, e.g. targeting
/// something created later). Only `linkpath` (where the new symlink node
/// goes) gets cwd-normalized; `target` is exactly what the caller passed,
/// same as real symlinks store whatever string they were given.
pub(super) fn sys_symlink(target_ptr: usize, linkpath_ptr: usize) -> SyscallResult {
    symlink_at(target_ptr, AT_FDCWD, linkpath_ptr)
}

/// symlinkat(266): `(target, newdirfd, linkpath)`.
pub(super) fn sys_symlinkat(target_ptr: usize, dirfd: i64, linkpath_ptr: usize) -> SyscallResult {
    symlink_at(target_ptr, dirfd, linkpath_ptr)
}

fn symlink_at(target_ptr: usize, dirfd: i64, linkpath_ptr: usize) -> SyscallResult {
    if let Err(e) = validate_user_buffer(target_ptr as u64, 1) { return e; }
    let target = read_user_str(target_ptr);
    if target.is_empty() { return errno::EINVAL; }
    let linkpath = match user_path_at(dirfd, linkpath_ptr, errno::EINVAL, rights::CAP_SYMLINKAT, "symlinkat", false) { Ok(p) => p, Err(e) => return e };
    match crate::fs::vfs::symlink(&target, &linkpath) {
        Ok(()) => 0,
        Err(e) => e.as_i64(),
    }
}

/// link(86): `(oldpath, newpath)`; a symlink at the end of `oldpath` is linked itself, not what it points to.
pub(super) fn sys_link(old_ptr: usize, new_ptr: usize) -> SyscallResult {
    link_at(AT_FDCWD, old_ptr, AT_FDCWD, new_ptr, 0)
}

/// linkat(265): `(olddirfd, oldpath, newdirfd, newpath, flags)`; `AT_SYMLINK_FOLLOW` follows a final symlink of `oldpath`.
pub(super) fn sys_linkat(olddirfd: i64, old_ptr: usize, newdirfd: i64, new_ptr: usize, flags: u64) -> SyscallResult {
    link_at(olddirfd, old_ptr, newdirfd, new_ptr, flags)
}

fn link_at(olddirfd: i64, old_ptr: usize, newdirfd: i64, new_ptr: usize, flags: u64) -> SyscallResult {
    const AT_SYMLINK_FOLLOW: u64 = 0x400;
    if flags & !AT_SYMLINK_FOLLOW != 0 {
        return errno::EINVAL;
    }
    let old_path = match user_path_at(olddirfd, old_ptr, errno::ENOENT, rights::CAP_LINKAT_SOURCE, "linkat", flags & AT_SYMLINK_FOLLOW != 0) { Ok(p) => p, Err(e) => return e };
    let new_path = match user_path_at(newdirfd, new_ptr, errno::ENOENT, rights::CAP_LINKAT_TARGET, "linkat", false) { Ok(p) => p, Err(e) => return e };
    match crate::fs::vfs::link(&old_path, &new_path, flags & AT_SYMLINK_FOLLOW != 0) {
        Ok(()) => 0,
        Err(e) => e.as_i64(),
    }
}

/// access(21): long access(const char *path, int mode)
///
/// `mode` is `F_OK` (0) or a bitmask of `R_OK`(4)/`W_OK`(2)/`X_OK`(1). This
/// kernel has no per-uid permission model, so R_OK/X_OK just mean "resolves
/// at all" (same as F_OK). W_OK needs a real answer, though: BusyBox `vi`
/// calls `access(path, W_OK)` to decide whether to open
/// `[Readonly]` — before this syscall existed at all it fell through the
/// dispatcher's default `ENOSYS`, which `vi` (correctly, defensively)
/// treats as "not writable", so every file — including ones on the
/// writable ramfs `/tmp` mount — opened readonly.
///
/// There's no `Inode`-level "is this writable" query to call instead
/// (writability is a property of the `FileHandle` returned by `open()`,
/// not the `Inode`), so this probes the same way a real write would: open
/// the path for writing, then issue a zero-length `write()`. Every
/// read-only filesystem's regular-file handle (initramfs, procfs)
/// unconditionally errors on `write()` regardless of buffer length, while
/// `RamFileHandle`'s and `Ext2FileHandle`'s `write()` with an empty
/// buffer are true no-ops — real answer, no side effect either way.
pub(super) fn sys_access(path_ptr: usize, mode: i32) -> SyscallResult {
    access_at(AT_FDCWD, path_ptr, mode)
}

/// faccessat(269) / faccessat2(439): `(dirfd, path, mode[, flags])`; the flags (`AT_EACCESS`, `AT_SYMLINK_NOFOLLOW`) change nothing
/// here, where there are no uids.
pub(super) fn sys_faccessat(dirfd: i64, path_ptr: usize, mode: i32) -> SyscallResult {
    access_at(dirfd, path_ptr, mode)
}

fn access_at(dirfd: i64, path_ptr: usize, mode: i32) -> SyscallResult {
    let path = match user_path_at(dirfd, path_ptr, errno::EINVAL, rights::CAP_FSTAT, "faccessat", true) { Ok(p) => p, Err(e) => return e };

    const W_OK: i32 = 2;

    if mode & W_OK != 0 {
        match crate::fs::vfs::open(&path, crate::fs::types::OpenFlags::WRONLY) {
            Ok(mut handle) => match handle.write(&[]) {
                Ok(_) => 0,
                Err(_) => errno::EACCES,
            },
            Err(e) => e.as_i64(),
        }
    } else {
        match crate::fs::stat(&path) {
            Ok(_) => 0,
            Err(e) => e.as_i64(),
        }
    }
}

/// rename(82): long rename(const char *old_path, const char *new_path)
pub(super) fn sys_rename(old_path_ptr: usize, new_path_ptr: usize) -> SyscallResult {
    rename_at(AT_FDCWD, old_path_ptr, AT_FDCWD, new_path_ptr)
}

/// renameat(264) / renameat2(316): `(olddirfd, old, newdirfd, new[, flags])`. Only `flags == 0` (no `RENAME_NOREPLACE` and friends).
pub(super) fn sys_renameat(olddirfd: i64, old_ptr: usize, newdirfd: i64, new_ptr: usize, flags: u64) -> SyscallResult {
    if flags != 0 {
        return errno::EINVAL;
    }
    rename_at(olddirfd, old_ptr, newdirfd, new_ptr)
}

fn rename_at(olddirfd: i64, old_path_ptr: usize, newdirfd: i64, new_path_ptr: usize) -> SyscallResult {
    let old_path = match user_path_at(olddirfd, old_path_ptr, errno::EINVAL, rights::CAP_RENAMEAT_SOURCE, "renameat", false) { Ok(p) => p, Err(e) => return e };
    let new_path = match user_path_at(newdirfd, new_path_ptr, errno::EINVAL, rights::CAP_RENAMEAT_TARGET, "renameat", false) { Ok(p) => p, Err(e) => return e };
    match crate::fs::vfs::rename(&old_path, &new_path) {
        Ok(())  => 0,
        Err(e)  => e.as_i64(),
    }
}

/// getcwd(79): long getcwd(char *buffer, size_t size)
///
/// This kernel's raw-syscall convention (unlike glibc's libc-level
/// `getcwd()`, which returns a `char*`) matches Linux's actual syscall:
/// returns the number of bytes written to `buffer` (including the NUL) on
/// success, or a negative errno. `ERANGE` if `size` is too small to hold
/// the current path + NUL.
pub(super) fn sys_getcwd(buf_ptr: usize, size: usize) -> SyscallResult {
    if size == 0 { return errno::EINVAL; }
    if let Err(e) = validate_user_buffer(buf_ptr as u64, size) { return e; }

    let cwd = current_cwd();
    let needed = cwd.len() + 1; // + NUL
    if needed > size {
        return errno::ERANGE;
    }

    unsafe {
        core::ptr::copy_nonoverlapping(cwd.as_ptr(), buf_ptr as *mut u8, cwd.len());
        *(buf_ptr as *mut u8).add(cwd.len()) = 0;
    }
    needed as SyscallResult
}

/// chdir(80): long chdir(const char *path)
///
/// Resolves `path` (relative to the current cwd if not absolute) and, if it
/// names an existing directory, replaces the process's cwd with the clean
/// normalized form — never the raw user string, so a later `getcwd()` never
/// echoes back `..`/`.`/double-slashes the caller happened to type.
pub(super) fn sys_chdir(path_ptr: usize) -> SyscallResult {
    if let Err(e) = validate_user_buffer(path_ptr as u64, 1) { return e; }
    let path = read_user_str(path_ptr);
    if path.is_empty() { return errno::EINVAL; }
    let path = resolve_path(path);

    let inode = match crate::fs::vfs::resolve(&path) {
        Ok(i)  => i,
        Err(e) => return e.as_i64(),
    };
    if inode.file_type() != crate::fs::types::FileType::Directory {
        return errno::ENOTDIR;
    }

    with_current_process(|proc| {
        proc.cwd = path;
        0
    })
}

/// fchdir(81): `chdir` to the directory an open fd names.
pub(super) fn sys_fchdir(fd: i32) -> SyscallResult {
    match dir_path_of(fd as i64, 0, "fchdir") {
        Ok((path, _)) => with_current_process(|proc| {
            proc.cwd = path;
            0
        }),
        Err(e) => e,
    }
}

/// The absolute path a `*at` call names: `ptr` is a user string, absolute or relative to `dirfd` (an open directory's fd, or
/// `AT_FDCWD` for the cwd). `empty_err` is the errno for an empty string. `EBADF` for a `dirfd` that is not open, `ENOTDIR` if it is
/// open but not a directory (or was not opened by path: pipes, sockets).
///
/// Capability rights: a relative path through a real `dirfd` needs `CAP_LOOKUP | need` on it (`ENOTCAPABLE`, logged for
/// `what`). An absolute path or `AT_FDCWD` uses no descriptor, so needs nothing (capability mode, stage 4, closes that).
///
/// Capability mode (`vfs::capmode`): `AT_FDCWD` and absolute paths are `ECAPMODE`; a relative path is walked beneath the
/// dirfd (`MountTable::resolve_at`, `RESOLVE_BENEATH`; `follow`: whether the call follows a final symlink) and the call uses
/// the canonical path that walk found, so neither `..` nor a symlink can leave the directory (`ENOTCAPABLE`). Every refusal
/// is recorded (`syscall::record_denial`).
fn user_path_at(dirfd: i64, ptr: usize, empty_err: i64, need: Rights, what: &str, follow: bool) -> Result<alloc::string::String, i64> {
    user_path_at_rights(dirfd, ptr, empty_err, need, what, follow).map(|(path, _)| path)
}

/// `user_path_at`, also returning the rights a descriptor opened by that path gets: the dirfd's when it went through one
/// (Capsicum), all of them otherwise.
fn user_path_at_rights(dirfd: i64, ptr: usize, empty_err: i64, need: Rights, what: &str, follow: bool) -> Result<(alloc::string::String, Rights), i64> {
    validate_user_buffer(ptr as u64, 1)?;
    let raw = read_user_str(ptr);
    if raw.is_empty() {
        return Err(empty_err);
    }
    let capmode = crate::process::scheduler::in_capmode();
    if raw.starts_with('/') || dirfd == AT_FDCWD {
        if capmode {
            let from = if raw.starts_with('/') { "/" } else { "the cwd" };
            return Err(super::record_denial(format_args!("{}: path '{}' from {}: ECAPMODE (capability mode)", what, raw, from), errno::ECAPMODE));
        }
        return Ok((resolve_path(raw), rights::CAP_ALL));
    }
    let (base, have) = dir_path_of(dirfd, rights::CAP_LOOKUP | need, what)?;
    if capmode {
        return match crate::fs::vfs::resolve_at(&base, raw, follow, vfs::resolve::RESOLVE_BENEATH) {
            Ok(vfs::mount::Walked::Found { path, .. }) | Ok(vfs::mount::Walked::Missing { path, .. }) => Ok((path, have)),
            Err(e) if e == crate::fs::types::Errno::EXDEV => Err(super::record_denial(
                format_args!("{}: path '{}' leaves directory fd {}: ENOTCAPABLE (capability mode)", what, raw, dirfd),
                errno::ENOTCAPABLE,
            )),
            Err(e) => Err(e.as_i64()),
        };
    }
    Ok((crate::fs::vfs::normalize_path(&base, raw), have))
}

/// `Ok` if open descriptor `fd` holds `need` (or is not open: the caller says `EBADF`), else `ENOTCAPABLE`, logged for `what`.
fn require_fd(fd: i64, need: Rights, what: &str) -> Result<(), i64> {
    if fd < 0 {
        return Ok(());
    }
    match with_fd_table(|t| t.check_rights(fd as usize, need)) {
        Some(Err(Some(right))) => Err(note_denied(format_args!("{}", what), fd, right)),
        _ => Ok(()),
    }
}

/// cap_rights_limit(405): `(fd, rights)`, narrow `fd`'s capability rights (`vfs::rights`). Only drops rights: asking for one
/// the fd lacks is `ENOTCAPABLE`, an unknown bit `EINVAL`, a closed fd `EBADF`. Not Linux (Linux has no Capsicum calls):
/// this kernel's number, after `statvfs` (404).
pub(super) fn sys_cap_rights_limit(fd: i32, want: u64) -> SyscallResult {
    if fd < 0 {
        return errno::EBADF;
    }
    with_files(|t| match t.limit_rights(fd as usize, want) {
        Ok(()) => 0,
        Err(e) => e.as_i64(),
    })
}

/// cap_rights_get(406): `(fd, *rights)`, write `fd`'s capability rights (a `u64`) to `out`.
pub(super) fn sys_cap_rights_get(fd: i32, out: u64) -> SyscallResult {
    if let Err(e) = validate_user_buffer(out, 8) {
        return e;
    }
    if fd < 0 {
        return errno::EBADF;
    }
    match with_fd_table(|t| t.rights(fd as usize).ok()).flatten() {
        Some(r) => {
            unsafe { core::ptr::write_unaligned(out as *mut u64, r) };
            0
        }
        None => errno::EBADF,
    }
}

/// The path directory descriptor `fd` was opened by (`FileDescriptorTable::path`), and its capability rights, which must
/// include `need` (`ENOTCAPABLE` otherwise, named in the log for `what`).
fn dir_path_of(fd: i64, need: Rights, what: &str) -> Result<(alloc::string::String, Rights), i64> {
    if fd < 0 {
        return Err(errno::EBADF);
    }
    let files = crate::process::irq_guard::SchedGuard::lock().running_ref().map(|p| p.files.clone()).ok_or(errno::ESRCH)?;
    let (open, path, have) = {
        let table = files.lock();
        (table.get(fd as usize).is_ok(), table.path(fd as usize).map(alloc::string::String::from), table.rights(fd as usize).unwrap_or(0))
    };
    if !open {
        return Err(errno::EBADF);
    }
    if let Some(right) = rights::first_missing(have, need) {
        return Err(note_denied(format_args!("{}", what), fd, right));
    }
    let path = path.ok_or(errno::ENOTDIR)?;
    match crate::fs::vfs::resolve(&path) {
        Ok(inode) if inode.file_type() == crate::fs::types::FileType::Directory => Ok((path, have)),
        Ok(_) => Err(errno::ENOTDIR),
        Err(e) => Err(e.as_i64()),
    }
}

/// getdents64(217): long getdents64(int fd, void *buf, size_t count)
///
/// Deliberately does NOT use `with_current_process`: that helper holds the
/// SCHEDULER lock across the whole closure, but `FileHandle::getdents64`
/// can need a fresh SCHEDULER lock of its own — `fs::procfs`'s `/proc`
/// listing (`ls /proc`, BusyBox `ps`'s `opendir("/proc")` scan) calls
/// `scheduler::all_pids()` to enumerate live pids, which self-deadlocks
/// (spin locks aren't reentrant) if SCHEDULER is already held on the way
/// in. Same shape as `sys_read`'s generic (fd > 0) path: clone the
/// `Arc<Mutex<FileDescriptorTable>>` under a short scheduler-lock scope,
/// release it, then call into the file outside any scheduler lock (cli
/// stays engaged throughout for the usual preemption-safety reasons, just
/// not the SCHEDULER mutex itself).
// utimensat(2) constants (Linux, `<fcntl.h>`/`<sys/stat.h>`).
const AT_FDCWD: i64 = -100;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const UTIME_NOW: i64 = (1 << 30) - 1;
const UTIME_OMIT: i64 = (1 << 30) - 2;

/// utimensat(280): `(dirfd, path, times[2], flags)`, Linux's signature.
/// `times` NULL means both now; a `tv_nsec` of `UTIME_NOW`/`UTIME_OMIT`
/// means now / leave alone; the change time becomes now either way. A NULL
/// `path` makes `dirfd` the file itself — that is how libc implements
/// `futimens(fd)`. `AT_SYMLINK_NOFOLLOW` sets a symlink's own times
/// (`lutimes`). Seconds only: no filesystem here keeps nanoseconds. A
/// relative path is resolved against `dirfd`'s recorded path (`user_path_at`).
pub(super) fn sys_utimensat(dirfd: i64, path_ptr: u64, times_ptr: u64, flags: u64) -> SyscallResult {
    if flags & !AT_SYMLINK_NOFOLLOW != 0 {
        return errno::EINVAL;
    }
    let now = crate::time::now_unix_secs();
    let (atime, mtime) = if times_ptr == 0 {
        (Some(now), Some(now))
    } else {
        if let Err(e) = validate_user_buffer(times_ptr, 32) { return e; }
        // Two `struct timespec`s: {tv_sec, tv_nsec} x {atime, mtime}.
        let t = unsafe { core::ptr::read_unaligned(times_ptr as *const [i64; 4]) };
        let pick = |sec: i64, nsec: i64| -> Result<Option<u64>, i64> {
            match nsec {
                UTIME_OMIT => Ok(None),
                UTIME_NOW => Ok(Some(now)),
                0..=999_999_999 if sec >= 0 => Ok(Some(sec as u64)),
                _ => Err(errno::EINVAL),
            }
        };
        match (pick(t[0], t[1]), pick(t[2], t[3])) {
            (Ok(a), Ok(m)) => (a, m),
            (Err(e), _) | (_, Err(e)) => return e,
        }
    };

    if path_ptr == 0 {
        if let Err(e) = require_fd(dirfd, rights::CAP_FUTIMES, "futimens") { return e; }
        // futimens: the fd's own file. The table is cloned out under the
        // scheduler lock and used after it is dropped — ext2 writes the
        // inode, and that is a disk (USB) transfer.
        let files = {
            let _irq = crate::process::irq_guard::InterruptGuard::new();
            let scheduler = crate::process::scheduler::local_scheduler();
            match scheduler.running_ref() {
                Some(proc) => proc.files.clone(),
                None => return errno::ESRCH,
            }
        };
        let mut table = files.lock();
        return match table.get_mut(dirfd as usize) {
            Err(_) => errno::EBADF,
            Ok(f) => match f.set_times(atime, mtime) {
                Ok(()) => 0,
                // Nothing behind the fd keeps times, or it is on a
                // read-only mount.
                Err(crate::process::file::FileError::NotSupported) => errno::EROFS,
                Err(_) => errno::EIO,
            },
        };
    }

    let path = match user_path_at(dirfd, path_ptr as usize, errno::ENOENT, rights::CAP_FUTIMES, "utimensat", flags & AT_SYMLINK_NOFOLLOW == 0) { Ok(p) => p, Err(e) => return e };
    let inode = if flags & AT_SYMLINK_NOFOLLOW != 0 {
        crate::fs::vfs::resolve_no_follow(&path)
    } else {
        crate::fs::vfs::resolve(&path)
    };
    match inode.and_then(|i| i.set_times(atime, mtime)) {
        Ok(()) => 0,
        Err(e) => e.as_i64(),
    }
}

pub(super) fn sys_getdents64(fd: i32, buf_ptr: usize, count: usize) -> SyscallResult {
    if let Err(e) = validate_user_buffer(buf_ptr as u64, count) { return e; }
    crate::ktrace!(crate::debug::FS, "sys_getdents64: fd={} count={}", fd, count);

    let _irq = crate::process::irq_guard::InterruptGuard::new();
    let files = {
        let scheduler = crate::process::scheduler::local_scheduler();
        match scheduler.running_ref() {
            Some(proc) => proc.files.clone(),
            None => return errno::ESRCH,
        }
    };

    let mut files_guard = files.lock();
    match files_guard.get_mut(fd as usize) {
        Err(_) => errno::EBADF,
        Ok(f)  => {
            let buf = unsafe {
                core::slice::from_raw_parts_mut(buf_ptr as *mut u8, count)
            };
            f.getdents64(buf)
        }
    }
}

/// sys_close — close a file descriptor.
///
/// Deliberately does NOT use `with_current_process`: that helper holds the
/// SCHEDULER lock across the whole closure, but closing a pipe end can drop
/// a `Box<dyn FileHandle>` whose `Drop` impl needs to wake a peer blocked on
/// the other end of the pipe (via `local_scheduler()` + `wake()`). Dropping
/// the handle while SCHEDULER is already held would self-deadlock (spin
/// locks aren't reentrant). Instead: clone the `Arc<Mutex<FileDescriptorTable>>`
/// under a short-lived [`irq_guard::SchedGuard`], let it drop (unlock, then
/// re-enable interrupts) at the block's closing brace, then close outside
/// any scheduler lock under a fresh [`irq_guard::InterruptGuard`] — same
/// shape sys_fork/sys_exec use for lock-crossing work.
///
/// HISTORY: this used to hand-pair `asm!("cli")`/`asm!("sti")`, and the
/// scheduler guard was once bound to a name in the same block as the
/// `sti()` call — so it didn't actually drop (release the lock) until the
/// block's closing brace, *after* `sti()` had already run. That reopened-
/// interrupts-but-still-locked window caused a real, reproducible
/// full-kernel hang: a timer tick landing in it found SCHEDULER held with
/// no way to ever release it (spin::Mutex isn't reentrant). `SchedGuard`
/// (see `irq_guard.rs`) makes that ordering mistake impossible to write:
/// unlock and `sti` now happen exactly at Rust's own scope-exit point.
/// Same fix applied to `sys_dup2` below, which had the identical shape.
pub(super) fn sys_close(fd: i32) -> SyscallResult {
    let files = {
        let guard = crate::process::irq_guard::SchedGuard::lock();
        guard.running_ref().map(|proc| proc.files.clone())
    };
    let files = match files {
        Some(f) => f,
        None => return errno::ESRCH,
    };

    // Fresh interrupt-disabled scope: closing a pipe end can run its Drop
    // impl (deallocating, possibly waking a peer via a fresh, independent
    // SCHEDULER lock/unlock — safe, since no lock is already held across
    // this). Without cli, nothing stops a timer tick from preempting
    // mid-close, saving this process's trapframe with cs = kernel (0x08)
    // instead of user (0x23) — and later treating that stale kernel-mode
    // snapshot as a live user context (e.g. for signal delivery, which
    // needs a genuine user rsp/rip) corrupts whatever that kernel rsp
    // actually pointed at.
    let _irq = crate::process::irq_guard::InterruptGuard::new();
    let result = files.lock().close(fd as usize);
    match result {
        Ok(_) => 0,
        Err(_) => errno::EBADF,
    }
}

/// dup(32): long dup(int fd)
///
/// Never closes anything (always lands on a *free* slot), so — unlike
/// dup2/close — there's no pipe-Drop-while-locked hazard here; plain
/// `with_current_process` is fine.
pub(super) fn sys_dup(fd: i32) -> SyscallResult {
    if fd < 0 { return errno::EBADF; }
    with_files(|files| {
        match files.dup(fd as usize, 0) {
            Ok(newfd) => newfd as SyscallResult,
            Err(_) => errno::EBADF,
        }
    })
}

/// dup2(33): long dup2(int oldfd, int newfd)
///
/// Same lock-dropping shape as `sys_close` (see its doc comment, including
/// the RAII-guard history): if `newfd` is already open, installing the dup
/// closes whatever was there first, which can run a pipe's Drop impl and
/// deadlock if SCHEDULER were still held.
pub(super) fn sys_dup2(oldfd: i32, newfd: i32) -> SyscallResult {
    if oldfd < 0 || newfd < 0 { return errno::EBADF; }

    let files = {
        let guard = crate::process::irq_guard::SchedGuard::lock();
        guard.running_ref().map(|proc| proc.files.clone())
    };
    let files = match files {
        Some(f) => f,
        None => return errno::ESRCH,
    };

    let _irq = crate::process::irq_guard::InterruptGuard::new();
    let result = files.lock().dup2(oldfd as usize, newfd as usize);
    match result {
        Ok(nf) => nf as SyscallResult,
        Err(_) => errno::EBADF,
    }
}

/// dup3(292): like dup2, but `oldfd == newfd` is `EINVAL` and the only flag is `O_CLOEXEC`, which the new descriptor gets.
pub(super) fn sys_dup3(oldfd: i32, newfd: i32, flags: i32) -> SyscallResult {
    if oldfd < 0 || newfd < 0 { return errno::EBADF; }
    if flags & !O_CLOEXEC != 0 || oldfd == newfd { return errno::EINVAL; }

    let files = {
        let guard = crate::process::irq_guard::SchedGuard::lock();
        guard.running_ref().map(|proc| proc.files.clone())
    };
    let Some(files) = files else { return errno::ESRCH };

    let _irq = crate::process::irq_guard::InterruptGuard::new();
    let result = files.lock().dup3(oldfd as usize, newfd as usize, flags & O_CLOEXEC != 0);
    match result {
        Ok(nf) => nf as SyscallResult,
        Err(_) => errno::EBADF,
    }
}

// fcntl(2) commands this kernel understands — real Linux x86-64 values.
const F_DUPFD: i32 = 0;
const F_GETFD: i32 = 1;
const F_SETFD: i32 = 2;
const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const F_DUPFD_CLOEXEC: i32 = 1030;
/// `F_GETFD`/`F_SETFD`'s one bit, and `open`/`pipe2`/`dup3`'s `O_CLOEXEC`.
const FD_CLOEXEC: u64 = 1;
pub(super) const O_CLOEXEC: i32 = 0o2000000;
/// `O_NONBLOCK`, the one status flag `F_SETFL` acts on for real.
const O_NONBLOCK: i64 = 0o4000;

/// fcntl(72): long fcntl(int fd, int cmd, unsigned long arg)
///
/// F_DUPFD/F_DUPFD_CLOEXEC and F_GETFL/F_SETFL's O_NONBLOCK do something;
/// the remaining commands are validity-checked stubs. F_DUPFD does the
/// same thing: this kernel has no per-fd close-on-exec flag anywhere, so
/// there's nothing for the CLOEXEC half to set differently. F_GETFD/
/// F_SETFD/F_GETFL/F_SETFL are stubbed — `FileDescriptorTable` has no
/// per-fd flags storage to back real answers with, so the getters always
/// report 0 and the setters silently accept anything (after checking `fd`
/// is actually open). Good enough for callers that only care whether the
/// call succeeded, not a real flags implementation.
pub(super) fn sys_fcntl(fd: i32, cmd: i32, arg: u64) -> SyscallResult {
    if fd < 0 { return errno::EBADF; }
    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            with_files(|files| {
                match files.dup_with(fd as usize, arg as usize, cmd == F_DUPFD_CLOEXEC) {
                    Ok(newfd) => newfd as SyscallResult,
                    Err(crate::process::file::FileError::InvalidArgument) => errno::EMFILE,
                    Err(_) => errno::EBADF,
                }
            })
        }
        // O_NONBLOCK is real for handles that support it (sockets today);
        // the rest of the status flags are still validity-checked stubs.
        F_GETFL => {
            with_files(|files| {
                match files.get(fd as usize) {
                    Ok(h) => if h.nonblocking() { O_NONBLOCK as i64 } else { 0 },
                    Err(_) => errno::EBADF,
                }
            })
        }
        F_SETFL => {
            with_files(|files| {
                match files.get(fd as usize) {
                    Ok(h) => {
                        let want = arg as i64 & O_NONBLOCK != 0;
                        // A handle with no notion of non-blocking silently
                        // ignoring the request would be worse than saying so
                        // — but only when the caller actually asked for it.
                        if !h.set_nonblocking(want) && want {
                            return errno::EINVAL;
                        }
                        0
                    }
                    Err(_) => errno::EBADF,
                }
            })
        }
        // FD_CLOEXEC is the descriptor's only flag; `exec` closes the ones that have it.
        F_GETFD => {
            with_files(|files| {
                match files.cloexec(fd as usize) {
                    Ok(on) => on as SyscallResult,
                    Err(_) => errno::EBADF,
                }
            })
        }
        F_SETFD => {
            with_files(|files| {
                match files.set_cloexec(fd as usize, arg & FD_CLOEXEC != 0) {
                    Ok(()) => 0,
                    Err(_) => errno::EBADF,
                }
            })
        }
        _ => errno::EINVAL,
    }
}

/// pipe(22): long pipe(int pipefd[2])
///
/// pipefd[0] = read end, pipefd[1] = write end (matches Linux). Both fds
/// start with one open reference; `fork()` duplicates them (see
/// `FileHandle::dup` / `FileDescriptorTable::clone`), `clone()` (threads)
/// shares them automatically via the shared fd table.
pub(super) fn sys_pipe(pipefd_ptr: u64) -> SyscallResult {
    sys_pipe2(pipefd_ptr, 0)
}

/// pipe2(293): `pipe` with `O_CLOEXEC` and `O_NONBLOCK` for both ends.
/// eventfd2(290) / eventfd(284, no flags): a counter fd, `EFD_SEMAPHORE | EFD_NONBLOCK | EFD_CLOEXEC` (`process::eventfd`).
pub(super) fn sys_eventfd(initval: u32, flags: i32) -> SyscallResult {
    use crate::process::eventfd::{EFD_CLOEXEC, EFD_NONBLOCK, EFD_SEMAPHORE};
    if flags & !(EFD_SEMAPHORE | EFD_NONBLOCK | EFD_CLOEXEC) != 0 {
        return errno::EINVAL;
    }
    let handle = crate::process::eventfd::create(initval, flags);
    with_files(|files| {
        match files.allocate(alloc::boxed::Box::new(handle)) {
            Ok(fd) => {
                if flags & EFD_CLOEXEC != 0 {
                    let _ = files.set_cloexec(fd, true);
                }
                fd as SyscallResult
            }
            Err(_) => errno::EMFILE,
        }
    })
}

pub(super) fn sys_pipe2(pipefd_ptr: u64, flags: i32) -> SyscallResult {
    if flags & !(O_CLOEXEC | O_NONBLOCK as i32) != 0 {
        return errno::EINVAL;
    }
    if let Err(e) = validate_user_buffer(pipefd_ptr, 8) {
        return e;
    }

    let (read_end, write_end) = crate::process::pipe::create();
    if flags & O_NONBLOCK as i32 != 0 {
        use crate::process::file::FileHandle;
        read_end.set_nonblocking(true);
        write_end.set_nonblocking(true);
    }

    with_files(|files| {
        let rfd = match files.allocate(alloc::boxed::Box::new(read_end)) {
            Ok(fd) => fd,
            Err(_) => return errno::EMFILE,
        };
        let wfd = match files.allocate(alloc::boxed::Box::new(write_end)) {
            Ok(fd) => fd,
            Err(_) => {
                // Rolling back by dropping the read end here (while SCHEDULER
                // is held via with_current_process) is safe ONLY because this
                // pipe was just created in this same call and has never been
                // exposed to another process — its write_waiters queue is always
                // empty, so PipeReadEnd::drop() cannot reach the wake path
                // that would need to re-lock SCHEDULER. Don't reuse this
                // pattern for closing an fd a process has actually had open.
                let _ = files.close(rfd);
                return errno::EMFILE;
            }
        };
        if flags & O_CLOEXEC != 0 {
            let _ = files.set_cloexec(rfd, true);
            let _ = files.set_cloexec(wfd, true);
        }
        drop(files);

        unsafe {
            let ptr = pipefd_ptr as *mut i32;
            ptr.write(rfd as i32);
            ptr.add(1).write(wfd as i32);
        }
        0
    })
}

/// mmap(9): void *mmap(void *addr, size_t length, int prot, int flags, int fd, off_t offset)
///
/// Two kinds of mapping:
/// - `MAP_PRIVATE|MAP_ANONYMOUS` (or plain `MAP_ANONYMOUS`), `fd == -1`:
///   private zero-filled memory, COW across `fork`.
/// - `MAP_SHARED`: the pages of a shared-memory object
///   (`memory::shm::ShmObject`) — a `memfd_create` fd's, or a fresh one for
///   `MAP_SHARED|MAP_ANONYMOUS`, which is then shared across `fork`.
///   `offset` must be page-aligned. An fd that is not a memfd is `ENODEV`
///   (files on ext2/ramfs cannot be mapped yet).
///
/// `MAP_PRIVATE` of an fd (a COW snapshot of a file) is not supported:
/// `EINVAL`. `addr != 0` is taken as `MAP_FIXED`, as before.
pub(super) fn sys_mmap(addr: u64, length: u64, prot: u32, flags: u32, fd: i32, offset: u64) -> SyscallResult {
    const MAP_SHARED: u32 = 0x01;
    const MAP_PRIVATE: u32 = 0x02;
    const MAP_ANONYMOUS: u32 = 0x20;
    let anon = flags & MAP_ANONYMOUS != 0;
    if flags & MAP_SHARED == 0 {
        if !anon || fd != -1 {
            return errno::EINVAL;
        }
        return with_current_process(|proc| {
            let r = proc.address_space.sys_mmap_anon(addr, length, prot);
            crate::ktrace!(crate::debug::MM, "mmap pid={:?} addr={:#x} len={:#x} -> {:?}", proc.pid, addr, length, r);
            match r {
                Ok(vaddr) => vaddr as i64,
                Err(_)    => errno::ENOMEM,
            }
        });
    }

    use crate::memory::shm::ShmObject;
    use alloc::sync::Arc;
    if flags & MAP_PRIVATE != 0 || offset & 0xFFF != 0 || length == 0 || addr & 0xFFF != 0 {
        return errno::EINVAL;
    }
    // The object first, from the fd table (never under `SCHEDULER`), then the mapping.
    let obj: Arc<ShmObject> = if anon {
        let obj = Arc::new(ShmObject::new());
        if obj.set_size(length).is_err() {
            return errno::ENOMEM;
        }
        obj
    } else {
        let found = with_fd_table(|files| {
            let Ok(handle) = files.get(fd as usize) else { return Err(errno::EBADF) };
            match handle.shm_object().map(|o| o.downcast::<ShmObject>()) {
                Some(Ok(obj)) => Ok(obj),
                _ => Err(errno::ENODEV),
            }
        });
        match found {
            Some(Ok(obj)) => obj,
            Some(Err(e)) => return e,
            None => return errno::ESRCH,
        }
    };
    with_current_process(|proc| {
        let r = proc.address_space.mmap_shared(addr, length, prot, obj, (offset / 4096) as usize);
        crate::ktrace!(crate::debug::MM, "mmap shared pid={:?} addr={:#x} len={:#x} fd={} off={:#x} -> {:?}",
            proc.pid, addr, length, fd, offset, r);
        match r {
            Ok(vaddr) => vaddr as i64,
            Err(_)    => errno::ENOMEM,
        }
    })
}

/// memfd_create(319): int memfd_create(const char *name, unsigned flags)
///
/// A new shared-memory object of size 0 behind a new fd (`ipc::memfd`).
/// `name` is only for Linux's `/proc/<pid>/fd` display, which this kernel
/// does not have, so it is not kept. `MFD_CLOEXEC` marks the fd close-on-exec; `MFD_ALLOW_SEALING` is
/// accepted, but seals themselves (`F_ADD_SEALS`) are not implemented.
pub(super) fn sys_memfd_create(name_ptr: u64, flags: u32) -> SyscallResult {
    const MFD_CLOEXEC: u32 = 1;
    const MFD_ALLOW_SEALING: u32 = 2;
    if flags & !(MFD_CLOEXEC | MFD_ALLOW_SEALING) != 0 {
        return errno::EINVAL;
    }
    if let Err(e) = validate_user_buffer(name_ptr, 1) {
        return e;
    }
    let handle = alloc::boxed::Box::new(crate::ipc::memfd::MemfdHandle::new());
    with_files(|files| {
        match files.allocate(handle) {
            Ok(fd) => {
                if flags & MFD_CLOEXEC != 0 {
                    let _ = files.set_cloexec(fd, true);
                }
                fd as i64
            }
            Err(_) => errno::EMFILE,
        }
    })
}

/// ftruncate(77): int ftruncate(int fd, off_t length)
///
/// Only memfds (`EINVAL` for anything else, Linux's answer for a
/// non-regular file). Growing is always allowed; shrinking a mapped object
/// is `EBUSY` (Linux allows it and `SIGBUS`es the mappings — see
/// `ShmObject::set_size`).
pub(super) fn sys_ftruncate(fd: i32, length: i64) -> SyscallResult {
    use crate::memory::shm::{ShmError, ShmObject};
    if length < 0 {
        return errno::EINVAL;
    }
    // Under the scheduler lock, like `lseek`; the object's own lock and
    // `BUDDY` (a shrink frees frames) come after it in the lock order.
    with_files(|files| {
        let Ok(handle) = files.get(fd as usize) else { return errno::EBADF };
        let obj = match handle.shm_object().map(|o| o.downcast::<ShmObject>()) {
            Some(Ok(obj)) => obj,
            _ => return errno::EINVAL,
        };
        match obj.set_size(length as u64) {
            Ok(()) => 0,
            Err(ShmError::TooBig) => errno::EFBIG,
            Err(ShmError::Busy) => errno::EBUSY,
            Err(ShmError::NoMemory) => errno::ENOMEM,
        }
    })
}

/// munmap(11): int munmap(void *addr, size_t length)
///
/// Any page-aligned range: it may cut a mapping in pieces, span several, or
/// include holes (`AddressSpace::sys_munmap`).
pub(super) fn sys_munmap(addr: u64, length: u64) -> SyscallResult {
    with_current_process(|proc| {
        let r = unsafe { proc.address_space.sys_munmap(addr, length) };
        crate::ktrace!(crate::debug::MM, "munmap pid={:?} addr={:#x} len={:#x} -> {:?}", proc.pid, addr, length, r);
        match r {
            Ok(())  => 0,
            Err(_)  => errno::EINVAL,
        }
    })
}

/// mprotect(10): int mprotect(void *addr, size_t length, int prot)
pub(super) fn sys_mprotect(addr: u64, length: u64, prot: u32) -> SyscallResult {
    use crate::memory::address_space::MprotectError;
    with_current_process(|proc| {
        let r = unsafe { proc.address_space.sys_mprotect(addr, length, prot) };
        crate::ktrace!(crate::debug::MM, "mprotect pid={:?} addr={:#x} len={:#x} prot={} -> {}",
            proc.pid, addr, length, prot, if r.is_ok() { "ok" } else { "err" });
        match r {
            Ok(()) => 0,
            Err(MprotectError::Invalid) => errno::EINVAL,
            Err(MprotectError::NoMem) => errno::ENOMEM,
            Err(MprotectError::Failed(_)) => errno::ENOMEM,
        }
    })
}

// ── lseek(8) ───────────────────────────────────────────────────────────────

/// lseek(8): off_t lseek(int fd, off_t offset, int whence)
///
/// Seeking on character devices (console, keyboard) is not meaningful;
/// return ESPIPE just like Linux does for pipes.  When we have a VFS,
/// this will delegate to the file's seek method.
/// lseek(8): off_t lseek(int fd, off_t offset, int whence)
///
/// Real seek support for regular-file handles (ramfs, initramfs, ext2 —
/// see their `FileHandle::seek` impls); character devices and pipes still
/// report `ESPIPE` via the trait's default. This used to be a blanket
/// stub returning `ESPIPE` unconditionally, written back when every fd
/// really was a character device — never updated once regular files
/// existed, which silently broke any program doing non-sequential reads
/// on a real file (confirmed live: `doom`'s WAD loader, which seeks
/// around a WAD's lump directory instead of reading it start-to-end).
pub(super) fn sys_lseek(fd: i32, offset: i64, whence: i32) -> SyscallResult {
    if fd < 0 || fd as usize >= crate::process::file::MAX_FILES { return errno::EBADF; }
    with_files(|files| {
        match files.get_mut(fd as usize) {
            Ok(file) => match file.seek(offset, whence) {
                Ok(pos) => pos,
                Err(crate::process::file::FileError::NotSupported) => errno::ESPIPE,
                Err(_) => errno::EINVAL,
            },
            Err(_) => errno::EBADF,
        }
    })
}

// ── brk(12) ────────────────────────────────────────────────────────────────

/// brk(12): int brk(void *addr)
///
/// Returning 0 (failure, current break unchanged) tells mlibc to fall
/// back to mmap(MAP_ANONYMOUS) for heap allocation, which we support.
pub(super) fn sys_brk(_addr: u64) -> SyscallResult {
    0
}

// ── ioctl(16) ──────────────────────────────────────────────────────────────

/// ioctl(16): int ioctl(int fd, unsigned long request, ...)
///
/// Backs mlibc's `sys_isatty` (via TCGETS with a null pointer — kept
/// working exactly as before), the real `tcgetattr`/`tcsetattr` sysdeps
/// hooks (which this port implements as thin TCGETS/TCSETS* wrappers, same
/// as real glibc does — see `mlibc-port/.../generic.cpp::sys_tcgetattr`),
/// `tcgetpgrp`/`tcsetpgrp` (TIOCGPGRP/TIOCSPGRP — mlibc calls `ioctl()`
/// directly for these, not a sysdeps hook), and terminal-size queries.
/// A blit request's fixed-size argument struct, written by userspace into
/// the buffer `FBIO_BLIT`'s `argp` points at: a pointer to its own
/// `0x00RRGGBB`-packed pixel buffer plus that buffer's dimensions. Matches
/// C layout so a C caller can just define the equivalent struct directly.
#[repr(C)]
struct FbBlitArgs {
    ptr: u64,
    width: u32,
    height: u32,
}

pub(super) fn sys_ioctl(fd: i32, request: u64, argp: u64) -> SyscallResult {
    const TCGETS: u64 = 0x5401;
    const TCSETS: u64 = 0x5402;
    const TCSETSW: u64 = 0x5403;
    const TCSETSF: u64 = 0x5404;
    const TIOCGWINSZ: u64 = 0x5413;
    const TIOCGPGRP: u64 = 0x540F;
    const TIOCSPGRP: u64 = 0x5410;
    const TCFLSH: u64 = 0x540B;
    // Custom, this-kernel-only request code (not a real Linux fbdev ioctl —
    // real fbdev exposes the framebuffer via mmap; we don't support
    // device-backed mmap, so a raw-pixel client instead hands us its own
    // offscreen buffer once per frame and we blit it in).
    const FBIO_BLIT: u64 = 0x4642_0001;

    if fd < 0 { return errno::EBADF; }

    // Device-specific requests first (`FileHandle::ioctl`, e.g. EVIOCGRAB
    // on /dev/input/event0). The fd table is cloned out and the scheduler
    // lock released before calling in, like sys_read's generic path.
    let files = {
        let sched = crate::process::irq_guard::SchedGuard::lock();
        match sched.running_ref() {
            Some(proc) => proc.files.clone(),
            None => return errno::ESRCH,
        }
    };
    let handled = {
        let mut table = files.lock();
        // A request that names another descriptor (an import): look that one up now, while the table is ours, and hand its object in.
        let peer = match table.get(fd as usize) {
            Ok(file) => match file.ioctl_fd_arg(request, argp) {
                Some(n) => match table.get(n) {
                    Ok(other) => other.device_ref(),
                    Err(_) => return errno::EBADF,
                },
                None => None,
            },
            Err(_) => return errno::EBADF,
        };
        match table.get_mut(fd as usize) {
            Ok(file) => match file.ioctl_ex(request, argp, peer) {
                Some(crate::process::file::IoctlOut::Value(v)) => Some(v),
                // The ioctl made a file (a buffer exported as a descriptor): it is installed here, because only here is the table at hand.
                Some(crate::process::file::IoctlOut::NewFile(h)) => Some(match table.allocate(h) {
                    Ok(n) => {
                        let _ = table.set_cloexec(n, true);
                        n as i64
                    }
                    Err(_) => errno::EMFILE,
                }),
                None => None,
            },
            Err(_) => return errno::EBADF,
        }
    };
    if let Some(r) = handled {
        return r;
    }

    // Requests that mean the same on every descriptor. (`FileHandle::ioctl` above got the first look: a pty has its own FIONBIO.)
    const FIONBIO: u64 = 0x5421;
    const FIOCLEX: u64 = 0x5451;
    const FIONCLEX: u64 = 0x5450;
    match request {
        FIONBIO => {
            if validate_user_buffer(argp, 4).is_err() {
                return errno::EFAULT;
            }
            let want = unsafe { *(argp as *const i32) } != 0;
            let r = match files.lock().get(fd as usize) {
                Ok(h) => if h.set_nonblocking(want) || !want { 0 } else { errno::ENOTTY },
                Err(_) => errno::EBADF,
            };
            return r;
        }
        FIOCLEX | FIONCLEX => {
            return match files.lock().set_cloexec(fd as usize, request == FIOCLEX) {
                Ok(()) => 0,
                Err(_) => errno::EBADF,
            };
        }
        _ => {}
    }
    drop(files);

    #[derive(Clone, Copy, PartialEq)]
    enum FdKind { Serial, Fb, Other }

    // Classify the driver backing `fd`, under the same cli/SCHEDULER-lock/
    // sti dance every other fd-identity check in this function uses (never
    // by fd number — see the `is_tty` doc below for why that breaks under
    // `dup`). An owned enum (not the handle's borrowed `&str` name) so the
    // result can outlive the lock guard it was computed under.
    let fd_kind = with_fd_table(|t| {
        t.get(fd as usize).ok().map(|f| match f.name() {
            "serial" => FdKind::Serial,
            "fb" => FdKind::Fb,
            _ => FdKind::Other,
        })
    })
    .flatten();

    // A handle counts as a tty if it's actually backed by the console
    // driver (serial or framebuffer) — checked by the handle's identity,
    // not by fd number. A fixed "fd <= 2" check breaks the moment a tty fd
    // gets dup'd to something higher, which is exactly what real job
    // control setup does: ash's `setjobctl()` (shell/ash.c) opens/falls
    // back to the console, then `fcntl(fd, F_DUPFD_CLOEXEC, 10)`s it to a
    // fd >= 10 before calling `tcgetpgrp()` on *that* fd — confirmed live,
    // this was silently sending ash down its "can't access tty, job
    // control turned off" fallback path.
    let is_tty = matches!(fd_kind, Some(FdKind::Serial) | Some(FdKind::Fb));

    match request {
        TCGETS => {
            if !is_tty { return errno::ENOTTY; }
            // `argp == 0` is `sys_isatty`'s "just probe the return code"
            // call — nothing to write, and that's fine.
            const SZ: usize = core::mem::size_of::<crate::tty::Termios>();
            if argp != 0 && validate_user_buffer(argp, SZ).is_ok() {
                let t = *crate::tty::TERMIOS.lock();
                unsafe { core::ptr::write(argp as *mut crate::tty::Termios, t); }
            }
            0
        }
        TCSETS | TCSETSW | TCSETSF => {
            if !is_tty { return errno::ENOTTY; }
            const SZ: usize = core::mem::size_of::<crate::tty::Termios>();
            if let Err(e) = validate_user_buffer(argp, SZ) { return e; }
            // TCSETSW/TCSETSF (drain-first / flush-first) collapse to the
            // same immediate apply as TCSETS: there's no real output queue
            // to drain and no queued-but-unread input beyond
            // `keyboard_buffer::KEYBOARD_BUFFER` worth discarding.
            let t = unsafe { core::ptr::read(argp as *const crate::tty::Termios) };
            *crate::tty::TERMIOS.lock() = t;
            0
        }
        TIOCGWINSZ => {
            if argp != 0 && validate_user_buffer(argp, 8).is_ok() {
                // struct winsize { ws_row, ws_col, ws_xpixel, ws_ypixel }
                // Real framebuffer text-grid geometry (falls back to 80x25
                // if there's no framebuffer, e.g. serial-only boot) — a
                // full-screen program like `vi` sizes its display from
                // this, so a hardcoded value left it unable to use more
                // than a corner of an actual (usually much bigger) screen.
                let (cols, rows) = crate::drivers::framebuffer_console::text_dimensions();
                let (xpix, ypix) = crate::drivers::framebuffer_console::pixel_dimensions();
                let ws = argp as *mut u16;
                unsafe {
                    *ws.add(0) = rows as u16;
                    *ws.add(1) = cols as u16;
                    *ws.add(2) = xpix.min(u16::MAX as usize) as u16;
                    *ws.add(3) = ypix.min(u16::MAX as usize) as u16;
                }
            }
            0
        }
        // The console's input is the shared keyboard ring, which nothing
        // here usefully discards: accepted and ignored, as before
        // `tcflush` reached the kernel at all (a pty really flushes).
        TCFLSH => if is_tty { 0 } else { errno::ENOTTY },
        TIOCGPGRP => {
            if !is_tty { return errno::ENOTTY; }
            if let Err(e) = validate_user_buffer(argp, 4) { return e; }
            let pgid = crate::tty::FOREGROUND_PGID.load(core::sync::atomic::Ordering::Relaxed);
            unsafe { *(argp as *mut i32) = pgid as i32; }
            0
        }
        TIOCSPGRP => {
            if !is_tty { return errno::ENOTTY; }
            if let Err(e) = validate_user_buffer(argp, 4) { return e; }
            let pgid = unsafe { *(argp as *const i32) };
            if pgid <= 0 { return errno::EINVAL; }
            crate::tty::FOREGROUND_PGID.store(pgid as u32, core::sync::atomic::Ordering::Relaxed);
            0
        }
        FBIO_BLIT => {
            if fd_kind != Some(FdKind::Fb) { return errno::ENOTTY; }
            // The screen belongs to `/dev/fb0`'s holder; drawing over it
            // would break what that one believes is on screen.
            if crate::drivers::framebuffer_console::in_graphics_mode() { return errno::EBUSY; }
            const SZ: usize = core::mem::size_of::<FbBlitArgs>();
            if let Err(e) = validate_user_buffer(argp, SZ) { return e; }
            let args = unsafe { core::ptr::read(argp as *const FbBlitArgs) };
            let (w, h) = (args.width as usize, args.height as usize);
            // Bound the claimed size before trusting it for the slice
            // length below — an unchecked w*h here is a user-controlled
            // out-of-bounds read.
            if w == 0 || h == 0 || w > 4096 || h > 4096 { return errno::EINVAL; }
            if let Err(e) = validate_user_buffer(args.ptr, w * h * 4) { return e; }
            let src = unsafe { core::slice::from_raw_parts(args.ptr as *const u32, w * h) };
            if let Some(fb) = crate::framebuffer::FRAMEBUFFER.lock().as_mut() {
                fb.blit_scaled(src, w, h);
            }
            // Bypasses the text console's cursor/char tracking entirely —
            // flag it so the next text write (e.g. the shell prompt after
            // DOOM exits) clears the screen first instead of drawing over
            // whatever frame was left on screen. See framebuffer_console.rs.
            crate::drivers::framebuffer_console::mark_raw_dirty();
            0
        }
        _ => errno::EINVAL,
    }
}

// ── writev(20) ─────────────────────────────────────────────────────────────

/// writev(20): ssize_t writev(int fd, const struct iovec *iov, int iovcnt)
///
/// Loops over the iovec array and calls sys_write for each segment.
/// struct iovec = { void *iov_base (8 bytes), size_t iov_len (8 bytes) }
pub(super) fn sys_writev(fd: i32, iov_ptr: u64, iovcnt: usize) -> SyscallResult {
    if iovcnt > 1024 { return errno::EINVAL; }
    if validate_user_buffer(iov_ptr, iovcnt * 16).is_err() {
        return errno::EFAULT;
    }

    let mut total: i64 = 0;
    for i in 0..iovcnt {
        let entry = (iov_ptr + i as u64 * 16) as *const u64;
        let (base, len) = unsafe { (*entry, *entry.add(1)) };
        if len == 0 { continue; }
        let n = sys_write(fd, base as usize, len as usize);
        if n < 0 { return n; }
        total += n;
    }
    total
}

/// `struct statvfs` (see `sysroot/usr/include/abi-bits/statvfs.h`) — 11
/// `unsigned long`/`fsblkcnt_t`/`fsfilcnt_t` fields, all `u64` on x86-64.
#[repr(C)]
struct Statvfs {
    f_bsize: u64,
    f_frsize: u64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_favail: u64,
    f_fsid: u64,
    f_flag: u64,
    f_namemax: u64,
}

/// sys_statvfs (custom #404): long statvfs(const char *path, struct statvfs *out)
///
/// Backs BusyBox `df` (`statvfs()`, POSIX — mlibc's `sys_fstatvfs` also routes here with a fixed `"/"`, see the sysdep). A path on
/// the ext2 mount (`/mnt`) reports that volume's own block and inode counts from its superblock. Every other mount is in memory
/// (ramfs, procfs, devfs, initramfs) and shares the one physical-memory pool (the buddy allocator), so those report the RAM
/// totals: live, not fabricated, but not a per-mount breakdown. `path` must resolve.
pub(super) fn sys_statvfs(path_ptr: usize, out_ptr: usize) -> SyscallResult {
    if let Err(e) = validate_user_buffer(path_ptr as u64, 1) { return e; }
    if let Err(e) = validate_user_buffer(out_ptr as u64, core::mem::size_of::<Statvfs>()) { return e; }
    let path = read_user_str(path_ptr);
    if path.is_empty() { return errno::EINVAL; }
    let path = resolve_path(path);
    if let Err(e) = crate::fs::stat(&path) {
        return e.as_i64();
    }

    let on_ext2 = path == "/mnt" || path.starts_with("/mnt/");
    let out = match on_ext2.then(crate::fs::ext2::usage).flatten() {
        Some(u) => Statvfs {
            f_bsize: u.block_size as u64,
            f_frsize: u.block_size as u64,
            f_blocks: u.blocks as u64,
            f_bfree: u.free_blocks as u64,
            f_bavail: u.free_blocks as u64,
            f_files: u.inodes as u64,
            f_ffree: u.free_inodes as u64,
            f_favail: u.free_inodes as u64,
            f_fsid: 0,
            f_flag: if crate::fs::ext2::is_read_only() { 1 } else { 0 }, // ST_RDONLY
            f_namemax: 255,
        },
        None => {
            const BLOCK: u64 = 4096;
            let (total, free) = crate::allocator::mem_stats();
            Statvfs {
                f_bsize: BLOCK,
                f_frsize: BLOCK,
                f_blocks: total / BLOCK,
                f_bfree: free / BLOCK,
                f_bavail: free / BLOCK,
                f_files: 0,
                f_ffree: 0,
                f_favail: 0,
                f_fsid: 0,
                f_flag: 0,
                f_namemax: 255,
            }
        }
    };
    unsafe { core::ptr::write(out_ptr as *mut Statvfs, out); }
    0
}

/// chmod(90): long chmod(const char *path, mode_t mode)
///
/// Most filesystems here have no real per-inode permission-bits storage
/// (see `Stat::regular`/`regular_writable` — permission there is a
/// hardcoded property of *which filesystem* a file lives on), so for them
/// `Inode::chmod`'s default `Ok(())` keeps this a "validity-checked stub"
/// — good enough for `chmod`/`tar -p` extraction to report success
/// instead of failing outright. `ext2` is the exception: it has a real
/// on-disk `i_mode` field, and `Ext2Inode::chmod` actually persists the
/// change there.
pub(super) fn sys_chmod(path_ptr: usize, mode: u32) -> SyscallResult {
    chmod_at(AT_FDCWD, path_ptr, mode)
}

/// fchmodat(268) / fchmodat2(452): `(dirfd, path, mode[, flags])`.
pub(super) fn sys_fchmodat(dirfd: i64, path_ptr: usize, mode: u32) -> SyscallResult {
    chmod_at(dirfd, path_ptr, mode)
}

fn chmod_at(dirfd: i64, path_ptr: usize, mode: u32) -> SyscallResult {
    let path = match user_path_at(dirfd, path_ptr, errno::EINVAL, rights::CAP_FCHMOD, "fchmodat", true) { Ok(p) => p, Err(e) => return e };
    match crate::fs::vfs::resolve(&path).and_then(|inode| inode.chmod(mode)) {
        Ok(()) => 0,
        Err(e) => e.as_i64(),
    }
}

/// fchmod(91): same reasoning as `sys_chmod`, just fd-addressed via
/// `FileHandle::chmod` instead of resolving a path to an `Inode`.
pub(super) fn sys_fchmod(fd: i32, mode: u32) -> SyscallResult {
    if fd < 0 { return errno::EBADF; }
    with_files(|files| {
        match files.get_mut(fd as usize) {
            Ok(file) => match file.chmod(mode) {
                Ok(()) => 0,
                Err(_) => errno::EIO,
            },
            Err(_) => errno::EBADF,
        }
    })
}
