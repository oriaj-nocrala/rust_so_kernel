// kernel/src/process/file.rs
//
// File descriptor infrastructure: trait + per-process FD table.
//
// Device implementations live in kernel/src/drivers/.
// The FileHandle trait is the only coupling point between processes
// and drivers.

use alloc::boxed::Box;

// `FileError`/`FileResult`/`compute_seek`/`FileHandle` now live in the
// standalone, host-testable `vfs` crate (`vfs/src/file.rs`, `cd vfs &&
// cargo test`) — see `docs/fs/vfs-extraction-plan.md`. This re-export
// exists so no `use crate::process::file::...` anywhere in the kernel has
// to change. `FileDescriptorTable` below stays here: it needs
// `crate::drivers` (the device registry) and `crate::serial_println!`
// (debug logging), neither of which `vfs` can reach.
pub use vfs::file::{compute_seek, FileError, FileHandle, FileResult};

// ============================================================================
// FILE DESCRIPTOR TABLE
// ============================================================================

/// Slots a process's descriptor table can grow to (Linux's default soft limit is 1024). `poll`'s and `epoll`'s per-process tables
/// are sized from it.
pub const MAX_FILES: usize = 256;

/// Per-process table of open file descriptors.
///
/// A `Vec` that grows on demand up to `MAX_FILES`, not an array: an inline `[Option<Box<..>>; 256]` is 4 KiB moved by value
/// through `Mutex::new`, `Arc::new` and `clone`, and at opt-level 0 that overflowed the boot stack (a double fault creating
/// PID 1), the way `VmaList` once did. A slot past the end is a free one; `new()` allocates nothing, which the scheduler
/// relies on when it swaps in an empty table under its lock.
pub struct FileDescriptorTable {
    files: alloc::vec::Vec<Option<Box<dyn FileHandle>>>,
    /// `FD_CLOEXEC` per slot: `exec` closes the flagged ones (`take_cloexec`). A property of the descriptor, not of the handle,
    /// so `dup` gives the copy a clear flag and `fork` copies it as it is. Always as long as `files`.
    cloexec: alloc::vec::Vec<bool>,
}

impl FileDescriptorTable {
    /// Create an empty table.
    pub const fn new() -> Self {
        Self { files: alloc::vec::Vec::new(), cloexec: alloc::vec::Vec::new() }
    }

    /// Make `fd` a valid slot index (growing the table with free slots). `false` if it is past `MAX_FILES`.
    fn ensure(&mut self, fd: usize) -> bool {
        if fd >= MAX_FILES {
            return false;
        }
        if fd >= self.files.len() {
            self.files.resize_with(fd + 1, || None);
            self.cloexec.resize(fd + 1, false);
        }
        true
    }

    /// Put `handle` at `fd` (free or not; the caller has dealt with whatever was there), with a clear close-on-exec flag.
    fn put(&mut self, fd: usize, handle: Box<dyn FileHandle>, cloexec: bool) {
        self.files[fd] = Some(handle);
        self.cloexec[fd] = cloexec;
    }

    /// Create a table with stdin/stdout/stderr pre-opened.
    /// Uses the driver registry to get default handles.
    pub fn new_with_stdio() -> Self {
        use crate::drivers;

        let mut table = Self::new();
        table.ensure(2);

        // FD 0: stdin — bound to the console (serial), same device as
        // stderr. `sys_read`'s fd==0 branch hardcodes reading straight from
        // the keyboard buffer regardless of which handle sits here, so this
        // choice never affected *reading* — but it does matter for
        // isatty()/tcgetattr()/ioctl(TCGETS): a real interactive shell
        // (e.g. BusyBox ash) checks `isatty(0) && isatty(1)` to decide
        // whether to consider itself interactive at all (print a banner,
        // prompt, enable job control...). Binding this to `/dev/null` (the
        // previous "for now" placeholder) made that check permanently
        // false, silently forcing every shell into non-interactive mode.
        table.files[0] = Some(drivers::open_device("/dev/console").ok()
            .unwrap_or_else(|| Box::new(NullFallback)));

        // FD 1: stdout (framebuffer)
        table.files[1] = Some(drivers::open_device("/dev/fb").ok()
            .unwrap_or_else(|| Box::new(NullFallback)));

        // FD 2: stderr (framebuffer, same as stdout). Used to be bound to
        // `/dev/console` (serial-only) — errors like `ash: clear: not
        // found` were then invisible on the actual screen, only visible by
        // grepping serial.log, since nothing mirrors fb output *back* to
        // serial's own writes. Binding it to `/dev/fb` instead means stderr
        // is on-screen like stdout, and still reaches serial.log too via
        // `framebuffer_console`'s own `mirror_to_serial`.
        table.files[2] = Some(drivers::open_device("/dev/fb").ok()
            .unwrap_or_else(|| Box::new(NullFallback)));

        table
    }

    /// Get a mutable file handle.
    pub fn get_mut(&mut self, fd: usize) -> FileResult<&mut (dyn FileHandle + '_)> {
        match self.files.get_mut(fd) {
            Some(Some(boxed)) => Ok(&mut **boxed),
            _ => Err(FileError::BadFileDescriptor),
        }
    }

    /// Get an immutable file handle.
    pub fn get(&self, fd: usize) -> FileResult<&(dyn FileHandle + '_)> {
        match self.files.get(fd) {
            Some(Some(boxed)) => Ok(&**boxed),
            _ => Err(FileError::BadFileDescriptor),
        }
    }

    /// Allocate the first free FD for a handle.  Returns the FD number.
    pub fn allocate(&mut self, handle: Box<dyn FileHandle>) -> FileResult<usize> {
        let fd = match self.files.iter().position(|slot| slot.is_none()) {
            Some(i) => i,
            None => self.files.len(),
        };
        if !self.ensure(fd) {
            return Err(FileError::InvalidArgument); // Too many files open
        }
        self.put(fd, handle, false);
        Ok(fd)
    }

    /// dup(2): install a clone of `fd`'s handle at the first free slot
    /// `>= min_fd`. Relies on `FileHandle::dup()` — fds backed by a handle
    /// that doesn't implement it (returns `None`) can't be dup'd; today
    /// that's only directory handles (opendir), which nothing needs to
    /// dup in practice.
    pub fn dup(&mut self, fd: usize, min_fd: usize) -> FileResult<usize> {
        self.dup_with(fd, min_fd, false)
    }

    /// `dup` with `F_DUPFD_CLOEXEC`'s choice of the new descriptor's flag.
    pub fn dup_with(&mut self, fd: usize, min_fd: usize, cloexec: bool) -> FileResult<usize> {
        let cloned = self.get(fd)?.dup().ok_or(FileError::NotSupported)?;

        for i in min_fd..MAX_FILES {
            if self.files.get(i).map_or(true, |slot| slot.is_none()) {
                self.ensure(i);
                self.put(i, cloned, cloexec);
                return Ok(i);
            }
        }
        Err(FileError::InvalidArgument) // no free fd
    }

    /// dup2(2): install a clone of `oldfd`'s handle at exactly `newfd`,
    /// closing whatever was already there first. `oldfd == newfd` is a
    /// POSIX-mandated no-op (returns `newfd` without touching anything),
    /// as long as `oldfd` is actually open. The new descriptor is not
    /// close-on-exec.
    pub fn dup2(&mut self, oldfd: usize, newfd: usize) -> FileResult<usize> {
        self.dup3(oldfd, newfd, false)
    }

    /// dup3(2)'s core (and dup2's): as `dup2`, with the new descriptor's `FD_CLOEXEC`. The caller rejects `oldfd == newfd` for
    /// dup3 itself; here that stays `dup2`'s no-op and leaves the flag alone.
    pub fn dup3(&mut self, oldfd: usize, newfd: usize, cloexec: bool) -> FileResult<usize> {
        if newfd >= MAX_FILES {
            return Err(FileError::BadFileDescriptor);
        }
        if oldfd == newfd {
            self.get(oldfd)?; // still must be a valid open fd
            return Ok(newfd);
        }

        let cloned = self.get(oldfd)?.dup().ok_or(FileError::NotSupported)?;

        self.ensure(newfd);
        if let Some(mut old) = self.files[newfd].take() {
            let _ = old.close();
        }
        self.put(newfd, cloned, cloexec);
        Ok(newfd)
    }

    /// `FD_CLOEXEC` of an open descriptor.
    pub fn cloexec(&self, fd: usize) -> FileResult<bool> {
        self.get(fd)?;
        Ok(self.cloexec[fd])
    }

    /// Set or clear `FD_CLOEXEC` of an open descriptor.
    pub fn set_cloexec(&mut self, fd: usize, on: bool) -> FileResult<()> {
        self.get(fd)?;
        self.cloexec[fd] = on;
        Ok(())
    }

    /// Take out every close-on-exec handle, for `exec`. The caller closes and drops them once it holds no scheduler lock (a
    /// handle's `Drop` may take it).
    pub fn take_cloexec(&mut self) -> alloc::vec::Vec<Box<dyn FileHandle>> {
        let mut out = alloc::vec::Vec::new();
        for i in 0..self.files.len() {
            if self.cloexec[i] {
                self.cloexec[i] = false;
                if let Some(h) = self.files[i].take() {
                    out.push(h);
                }
            }
        }
        out
    }

    /// Close a file descriptor.
    pub fn close(&mut self, fd: usize) -> FileResult<()> {
        if fd >= MAX_FILES {
            return Err(FileError::BadFileDescriptor);
        }
        if fd >= self.files.len() {
            return Ok(());
        }

        self.cloexec[fd] = false;
        if let Some(mut handle) = self.files[fd].take() {
            handle.close()?;
        }

        Ok(())
    }

    /// The number of slots that can hold an open fd: one past the highest open one, or 0.
    pub fn open_extent(&self) -> usize {
        self.files.iter().rposition(|slot| slot.is_some()).map_or(0, |i| i + 1)
    }

    /// Debug: list all open FDs to serial.
    pub fn debug_list(&self) {
        crate::serial_println!("Open file descriptors:");
        for (i, slot) in self.files.iter().enumerate() {
            if let Some(handle) = slot {
                crate::serial_println!("  FD {}: {}", i, handle.name());
            }
        }
    }
}

// Fallback if driver registry isn't available (shouldn't happen)
struct NullFallback;
impl FileHandle for NullFallback {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> { Ok(0) }
    fn write(&mut self, buf: &[u8]) -> FileResult<usize> { Ok(buf.len()) }
    fn name(&self) -> &str { "<fallback>" }
}

// Every fd, including 0-2, is inherited via `dup()` when the underlying
// handle supports it (e.g. pipe ends, redirected regular files) — needed so
// a pipe created before `fork()` is usable by both parent and child, and so
// a shell redirect (`< file`, done via `open()`+`dup2()` onto fd 0 before
// `fork()`) survives into the child instead of silently reverting to the
// real console. fds 0-2 fall back to a fresh stdio handle only when nothing
// is open there, or the open handle doesn't support `dup()`.
impl Clone for FileDescriptorTable {
    fn clone(&self) -> Self {
        let mut new_table = Self::new();
        if self.files.is_empty() {
            return new_table;
        }
        new_table.ensure(self.files.len() - 1);

        for i in 0..self.files.len() {
            let Some(ref handle) = self.files[i] else { continue };
            new_table.files[i] = match i {
                0 => handle.dup().or_else(|| crate::drivers::open_device("/dev/console").ok()),
                1 | 2 => handle.dup().or_else(|| crate::drivers::open_device("/dev/fb").ok()),
                _ => handle.dup(),
            };
        }
        new_table.cloexec = self.cloexec.clone();

        new_table
    }
}
