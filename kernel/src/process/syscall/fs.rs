// kernel/src/process/syscall/fs.rs
//
// File/fd/path syscalls: read/write/open/close/stat family/getdents64/
// lseek/mmap/munmap/pipe/dup/dup2/fcntl/ioctl/writev/access/rename/mkdir/
// rmdir/unlink/symlink/readlink/chmod/fchmod/statvfs/getcwd/chdir, plus the
// stdin blocking-read machinery (keyboard ISR wakeup path).

use spin::Mutex;
use crate::process::TrapFrame;
use super::{
    errno, SyscallResult, with_current_process, validate_user_buffer,
    resolve_path, current_cwd, read_user_str, current_tf_ptr, CURRENT_SYSCALL_TF,
};
use core::sync::atomic::Ordering;

struct StdinWaiter {
    pid: usize,
    user_buf: u64,
}

static STDIN_WAITER: Mutex<Option<StdinWaiter>> = Mutex::new(None);

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
    let guard = crate::process::irq_guard::SchedGuard::lock();
    match guard.running_ref() {
        Some(proc) => proc.files.lock().get(0)
            .map(|h| h.name() == "serial")
            .unwrap_or(false),
        None => false,
    }
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
        let pid = crate::process::scheduler::current_pid().unwrap_or(0);
        *STDIN_WAITER.lock() = Some(StdinWaiter { pid, user_buf: buf as u64 });
        let tf_ptr = CURRENT_SYSCALL_TF.load(Ordering::Relaxed) as *const TrapFrame;
        block_stdin_read(tf_ptr)
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
            Err(crate::process::file::FileError::WouldBlock) => {
                let tf_ptr = current_tf_ptr();
                let next_tf = {
                    let mut scheduler = crate::process::scheduler::local_scheduler();
                    scheduler.block_current(tf_ptr)
                };
                unsafe { crate::process::trapframe::jump_to_user(next_tf) }
            }
            Err(_) => errno::EIO,
        }
    }
}

/// Block the calling process waiting for keyboard input.
///
/// cli must already be in effect when this is called.
/// Saves the current TrapFrame into the process Box, moves the process to the
/// wait_queue, and jumps to the next Ready process.  Never returns.
fn block_stdin_read(current_tf: *const TrapFrame) -> ! {
    let next_tf = {
        let mut sched = crate::process::scheduler::local_scheduler();
        sched.block_current(current_tf)
        // Lock dropped here; sti happens via iretq of the next process.
    };
    unsafe { crate::process::trapframe::jump_to_user(next_tf) }
}

/// Called by the keyboard ISR after a key is pushed into the buffer.
///
/// If a process is blocked on stdin, delivers the character to its user
/// buffer (via physical-memory translation), sets rax=1 in its saved
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

    // Consume the character that was just pushed by the keyboard ISR.
    let Some(c) = crate::keyboard::read_key() else {
        // Shouldn't happen (ISR pushed it just before calling us), but be safe.
        *STDIN_WAITER.lock() = Some(waiter);
        return;
    };

    let phys_offset = crate::memory::physical_memory_offset();
    let user_buf = waiter.user_buf;
    let pid = waiter.pid;

    let mut sched = crate::process::scheduler::local_scheduler();

    // Find the blocked process, translate its user buffer to a kernel VA,
    // write the character, and set rax=1 as the syscall return value.
    for proc in sched.wait_queue.iter_mut() {
        if proc.pid.0 == pid && matches!(proc.state, crate::process::ProcessState::Blocked) {
            use x86_64::{VirtAddr, structures::paging::{Page, Size4KiB}};

            let page = Page::<Size4KiB>::containing_address(VirtAddr::new(user_buf));
            let offset = user_buf & 0xFFF;

            if let Some(frame) = unsafe { proc.address_space.translate_page(page) } {
                let dst = phys_offset + frame.start_address().as_u64() + offset;
                unsafe { *(dst.as_mut_ptr::<u8>()) = c as u8; }
                proc.trapframe.rax = 1; // syscall return value: 1 byte read
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
        Err(crate::process::file::FileError::BrokenPipe) => errno::EPIPE,
        Err(crate::process::file::FileError::NoSpace) => errno::ENOSPC,
        Err(crate::process::file::FileError::WouldBlock) => {
            let tf_ptr = current_tf_ptr();
            let next_tf = {
                let mut scheduler = crate::process::scheduler::local_scheduler();
                scheduler.block_current(tf_ptr)
            };
            unsafe { crate::process::trapframe::jump_to_user(next_tf) }
        }
        Err(_) => errno::EIO,
    }
}

pub(super) fn sys_open(path_ptr: usize, flags: i32) -> SyscallResult {
    // Validation BEFORE cli — no lock needed
    if let Err(e) = validate_user_buffer(path_ptr as u64, 256) {
        return e;
    }

    let path = read_user_str(path_ptr);
    if path.is_empty() { return errno::EINVAL; }
    let path = resolve_path(path);
    crate::ktrace!(crate::debug::FS, "sys_open: path={} flags={:#x}", path, flags);

    // Resolve through VFS: /dev/* → drivers, /bin/* → initramfs, …
    // Box allocation uses Slab (different lock from SCHEDULER).
    let handle = match crate::fs::vfs::open(&path, crate::fs::types::OpenFlags(flags)) {
        Ok(h)  => h,
        Err(e) => { crate::ktrace!(crate::debug::FS, "sys_open: {} -> Err({:?})", path, e); return e.as_i64(); }
    };

    // Only take scheduler lock for the FD table insertion
    with_current_process(|proc| {
        match proc.files.lock().allocate(handle) {
            Ok(fd) => fd as i64,
            Err(_) => errno::EINVAL,
        }
    })
}

pub(super) fn sys_stat(path_ptr: usize, stat_ptr: usize) -> SyscallResult {
    stat_impl(path_ptr, stat_ptr, true)
}

/// lstat(6): like `stat`, but doesn't follow a symlink at the final path
/// component — reports the link itself (`FileType::Symlink`, `st_size` =
/// target length). Used to just alias `sys_stat` outright ("no symlinks
/// yet"); now that `fs::vfs` has real symlink support (see `fs::procfs`),
/// this is the genuine no-follow lookup.
pub(super) fn sys_lstat(path_ptr: usize, stat_ptr: usize) -> SyscallResult {
    stat_impl(path_ptr, stat_ptr, false)
}

fn stat_impl(path_ptr: usize, stat_ptr: usize, follow: bool) -> SyscallResult {
    use crate::fs::types::Stat;
    if let Err(e) = validate_user_buffer(path_ptr as u64, 1) { return e; }
    if let Err(e) = validate_user_buffer(stat_ptr as u64, core::mem::size_of::<Stat>()) { return e; }

    let path = read_user_str(path_ptr);
    let path = resolve_path(path);
    let result = if follow { crate::fs::stat(&path) } else { crate::fs::lstat(&path) };
    match result {
        Err(e)   => e.as_i64(),
        Ok(stat) => {
            unsafe { core::ptr::write(stat_ptr as *mut Stat, stat); }
            0
        }
    }
}

pub(super) fn sys_fstat(fd: i32, stat_ptr: usize) -> SyscallResult {
    use crate::fs::types::Stat;
    if let Err(e) = validate_user_buffer(stat_ptr as u64, core::mem::size_of::<Stat>()) { return e; }

    // Retrieve stat outside with_current_process to avoid holding the scheduler lock
    // while doing a potentially expensive write.
    let stat_result: Option<Stat> = {
        let mut sched = crate::process::scheduler::local_scheduler();
        sched.running_mut().and_then(|proc| {
            proc.files.lock().get(fd as usize).ok().and_then(|f| f.stat())
        })
    };

    match stat_result {
        None       => errno::EBADF,
        Some(stat) => {
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
    if let Err(e) = validate_user_buffer(path_ptr as u64, 1) { return e; }
    let path = read_user_str(path_ptr);
    if path.is_empty() { return errno::EINVAL; }
    let path = resolve_path(path);
    match crate::fs::vfs::mkdir(&path) {
        Ok(())  => 0,
        Err(e)  => e.as_i64(),
    }
}

/// rmdir(84): long rmdir(const char *path)
pub(super) fn sys_rmdir(path_ptr: usize) -> SyscallResult {
    if let Err(e) = validate_user_buffer(path_ptr as u64, 1) { return e; }
    let path = read_user_str(path_ptr);
    if path.is_empty() { return errno::EINVAL; }
    let path = resolve_path(path);
    match crate::fs::vfs::rmdir(&path) {
        Ok(())  => 0,
        Err(e)  => e.as_i64(),
    }
}

/// unlink(87): long unlink(const char *path)
pub(super) fn sys_unlink(path_ptr: usize) -> SyscallResult {
    if let Err(e) = validate_user_buffer(path_ptr as u64, 1) { return e; }
    let path = read_user_str(path_ptr);
    if path.is_empty() { return errno::EINVAL; }
    let path = resolve_path(path);
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
    if let Err(e) = validate_user_buffer(path_ptr as u64, 1) { return e; }
    if let Err(e) = validate_user_buffer(buf_ptr as u64, bufsiz) { return e; }
    let path = read_user_str(path_ptr);
    if path.is_empty() { return errno::EINVAL; }
    let path = resolve_path(path);
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
    if let Err(e) = validate_user_buffer(target_ptr as u64, 1) { return e; }
    if let Err(e) = validate_user_buffer(linkpath_ptr as u64, 1) { return e; }
    let target = read_user_str(target_ptr);
    let linkpath = read_user_str(linkpath_ptr);
    if target.is_empty() || linkpath.is_empty() { return errno::EINVAL; }
    let linkpath = resolve_path(linkpath);
    match crate::fs::vfs::symlink(&target, &linkpath) {
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
    if let Err(e) = validate_user_buffer(path_ptr as u64, 1) { return e; }
    let path = read_user_str(path_ptr);
    if path.is_empty() { return errno::EINVAL; }
    let path = resolve_path(path);

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
    if let Err(e) = validate_user_buffer(old_path_ptr as u64, 1) { return e; }
    if let Err(e) = validate_user_buffer(new_path_ptr as u64, 1) { return e; }
    let old_path = read_user_str(old_path_ptr);
    let new_path = read_user_str(new_path_ptr);
    if old_path.is_empty() || new_path.is_empty() { return errno::EINVAL; }
    let old_path = resolve_path(old_path);
    let new_path = resolve_path(new_path);
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
    with_current_process(|proc| {
        match proc.files.lock().dup(fd as usize, 0) {
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

// fcntl(2) commands this kernel understands — real Linux x86-64 values.
const F_DUPFD: i32 = 0;
const F_GETFD: i32 = 1;
const F_SETFD: i32 = 2;
const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const F_DUPFD_CLOEXEC: i32 = 1030;

/// fcntl(72): long fcntl(int fd, int cmd, unsigned long arg)
///
/// Only F_DUPFD/F_DUPFD_CLOEXEC actually do something, and they do the
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
            with_current_process(|proc| {
                match proc.files.lock().dup(fd as usize, arg as usize) {
                    Ok(newfd) => newfd as SyscallResult,
                    Err(_) => errno::EBADF,
                }
            })
        }
        F_GETFD | F_SETFD | F_GETFL | F_SETFL => {
            with_current_process(|proc| {
                match proc.files.lock().get(fd as usize) {
                    Ok(_)  => 0,
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
    if let Err(e) = validate_user_buffer(pipefd_ptr, 8) {
        return e;
    }

    let (read_end, write_end) = crate::process::pipe::create();

    with_current_process(|proc| {
        let mut files = proc.files.lock();
        let rfd = match files.allocate(alloc::boxed::Box::new(read_end)) {
            Ok(fd) => fd,
            Err(_) => return errno::EINVAL,
        };
        let wfd = match files.allocate(alloc::boxed::Box::new(write_end)) {
            Ok(fd) => fd,
            Err(_) => {
                // Rolling back by dropping the read end here (while SCHEDULER
                // is held via with_current_process) is safe ONLY because this
                // pipe was just created in this same call and has never been
                // exposed to another process — its write_waiter is always
                // None, so PipeReadEnd::drop() cannot reach the wake path
                // that would need to re-lock SCHEDULER. Don't reuse this
                // pattern for closing an fd a process has actually had open.
                let _ = files.close(rfd);
                return errno::EINVAL;
            }
        };
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
/// Only MAP_ANONYMOUS (0x20) is supported.  fd must be -1.
/// Returns the mapped virtual address on success, or ENOMEM / EINVAL.
pub(super) fn sys_mmap(addr: u64, length: u64, prot: u32, flags: u32, fd: i32) -> SyscallResult {
    const MAP_ANONYMOUS: u32 = 0x20;
    if flags & MAP_ANONYMOUS == 0 || fd != -1 {
        return errno::EINVAL;
    }
    with_current_process(|proc| {
        match proc.address_space.sys_mmap_anon(addr, length, prot) {
            Ok(vaddr) => vaddr as i64,
            Err(_)    => errno::ENOMEM,
        }
    })
}

/// munmap(11): int munmap(void *addr, size_t length)
///
/// Removes the VMA at `addr` and frees any demand-paged frames.
/// Requires exact match on addr and length (no partial unmap).
pub(super) fn sys_munmap(addr: u64, length: u64) -> SyscallResult {
    with_current_process(|proc| {
        match unsafe { proc.address_space.sys_munmap(addr, length) } {
            Ok(())  => 0,
            Err(_)  => errno::EINVAL,
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
    if fd < 0 || fd >= 16 { return errno::EBADF; }
    with_current_process(|proc| {
        match proc.files.lock().get_mut(fd as usize) {
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
    // Custom, this-kernel-only request code (not a real Linux fbdev ioctl —
    // real fbdev exposes the framebuffer via mmap; we don't support
    // device-backed mmap, so a raw-pixel client instead hands us its own
    // offscreen buffer once per frame and we blit it in).
    const FBIO_BLIT: u64 = 0x4642_0001;

    if fd < 0 { return errno::EBADF; }

    #[derive(Clone, Copy, PartialEq)]
    enum FdKind { Serial, Fb, Other }

    // Classify the driver backing `fd`, under the same cli/SCHEDULER-lock/
    // sti dance every other fd-identity check in this function uses (never
    // by fd number — see the `is_tty` doc below for why that breaks under
    // `dup`). An owned enum (not the handle's borrowed `&str` name) so the
    // result can outlive the lock guard it was computed under.
    let fd_kind = {
        let mut sched = crate::process::irq_guard::SchedGuard::lock();
        sched.running_mut().and_then(|proc| {
            proc.files.lock().get(fd as usize).ok().map(|f| match f.name() {
                "serial" => FdKind::Serial,
                "fb" => FdKind::Fb,
                _ => FdKind::Other,
            })
        })
    };

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
                let ws = argp as *mut u16;
                unsafe {
                    *ws.add(0) = rows as u16;
                    *ws.add(1) = cols as u16;
                    *ws.add(2) = 0;
                    *ws.add(3) = 0;
                }
            }
            0
        }
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
/// Backs BusyBox `df` (`statvfs()`, POSIX — mlibc's `sys_fstatvfs` also
/// routes here with a fixed `"/"`, see the sysdep). This kernel has one
/// physical-memory pool (the Buddy allocator) behind every mount rather
/// than real per-filesystem block accounting, so every path reports the
/// same numbers — enough for `df` to run and print plausible, live
/// (not fabricated-constant) total/free figures, not a real per-mount
/// breakdown. `path` only needs to resolve; the numbers don't depend on
/// what it resolves to.
pub(super) fn sys_statvfs(path_ptr: usize, out_ptr: usize) -> SyscallResult {
    if let Err(e) = validate_user_buffer(path_ptr as u64, 1) { return e; }
    if let Err(e) = validate_user_buffer(out_ptr as u64, core::mem::size_of::<Statvfs>()) { return e; }
    let path = read_user_str(path_ptr);
    if path.is_empty() { return errno::EINVAL; }
    let path = resolve_path(path);
    if let Err(e) = crate::fs::stat(&path) {
        return e.as_i64();
    }

    const BLOCK: u64 = 4096;
    let buddy = crate::allocator::buddy_allocator::BUDDY.lock();
    let total_blocks = buddy.total_bytes() as u64 / BLOCK;
    let free_blocks = buddy.free_bytes() as u64 / BLOCK;
    drop(buddy);

    let out = Statvfs {
        f_bsize: BLOCK,
        f_frsize: BLOCK,
        f_blocks: total_blocks,
        f_bfree: free_blocks,
        f_bavail: free_blocks,
        f_files: 0,
        f_ffree: 0,
        f_favail: 0,
        f_fsid: 0,
        f_flag: 0,
        f_namemax: 255,
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
    if let Err(e) = validate_user_buffer(path_ptr as u64, 1) { return e; }
    let path = read_user_str(path_ptr);
    if path.is_empty() { return errno::EINVAL; }
    let path = resolve_path(path);
    match crate::fs::vfs::resolve(&path).and_then(|inode| inode.chmod(mode)) {
        Ok(()) => 0,
        Err(e) => e.as_i64(),
    }
}

/// fchmod(91): same reasoning as `sys_chmod`, just fd-addressed via
/// `FileHandle::chmod` instead of resolving a path to an `Inode`.
pub(super) fn sys_fchmod(fd: i32, mode: u32) -> SyscallResult {
    if fd < 0 { return errno::EBADF; }
    with_current_process(|proc| {
        match proc.files.lock().get_mut(fd as usize) {
            Ok(file) => match file.chmod(mode) {
                Ok(()) => 0,
                Err(_) => errno::EIO,
            },
            Err(_) => errno::EBADF,
        }
    })
}
