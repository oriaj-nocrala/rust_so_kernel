// kernel/src/process/file.rs
//
// File descriptor infrastructure: trait + per-process FD table.
//
// Device implementations live in kernel/src/drivers/.
// The FileHandle trait is the only coupling point between processes
// and drivers.

use alloc::boxed::Box;

// ============================================================================
// ERRORS
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileError {
    BadFileDescriptor,
    InvalidArgument,
    IOError,
    NotSupported,
    EndOfFile,
    /// Write to a pipe with no open read ends (maps to EPIPE, and the
    /// caller additionally raises SIGPIPE — see `pipe.rs`/`sys_write`).
    BrokenPipe,
    /// Backing store (ext2 block/inode bitmap) is full — maps to ENOSPC,
    /// distinct from `IOError` so `sys_write` can report the real reason a
    /// write to a disk-backed filesystem failed.
    NoSpace,
    /// The operation would block (empty pipe on read, full pipe on write).
    /// `sys_read`/`sys_write` catch this, drop the fd-table lock, and
    /// perform the actual block_current/jump_to_trapframe themselves — see
    /// their doc comments for why this can't happen inside `read`/`write`.
    WouldBlock,
}

pub type FileResult<T> = Result<T, FileError>;

/// Shared `lseek(2)` offset arithmetic for regular-file handles (ramfs,
/// initramfs, ext2) — same SEEK_SET/SEEK_CUR/SEEK_END semantics, only the
/// "current position" and "file size" inputs differ per filesystem.
/// Negative results (seeking before byte 0) are rejected; seeking past
/// EOF is allowed (real `lseek` permits it — the next `read()` just
/// returns 0, or, for filesystems with write support, a later `write()`
/// there would create a hole).
pub fn compute_seek(current: i64, size: i64, offset: i64, whence: i32) -> FileResult<i64> {
    const SEEK_SET: i32 = 0;
    const SEEK_CUR: i32 = 1;
    const SEEK_END: i32 = 2;

    let base = match whence {
        SEEK_SET => 0,
        SEEK_CUR => current,
        SEEK_END => size,
        _ => return Err(FileError::InvalidArgument),
    };
    let new_pos = base.checked_add(offset).ok_or(FileError::InvalidArgument)?;
    if new_pos < 0 {
        return Err(FileError::InvalidArgument);
    }
    Ok(new_pos)
}

// ============================================================================
// TRAIT: FileHandle
// ============================================================================

/// Trait representing any "file" in the system.
///
/// Implementations include device drivers (/dev/null, /dev/console, etc.),
/// VFS-opened files (initramfs, future ext2), pipes, sockets, etc.
///
/// Optional VFS extensions (`stat`, `getdents64`) have default implementations
/// that are safe to ignore by device drivers.
pub trait FileHandle: Send {
    /// Read up to `buf.len()` bytes.  Returns bytes read.
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize>;

    /// Write up to `buf.len()` bytes.  Returns bytes written.
    fn write(&mut self, buf: &[u8]) -> FileResult<usize>;

    /// Close the file (optional, default no-op).
    fn close(&mut self) -> FileResult<()> {
        Ok(())
    }

    /// Return file metadata.  `None` for handles that don't support stat
    /// (e.g. legacy device handles opened before the VFS was initialised).
    fn stat(&self) -> Option<crate::fs::types::Stat> {
        None
    }

    /// Fill `buf` with `linux_dirent64` records.  Returns bytes written, or a
    /// negative errno on error.  Default returns `-ENOTDIR` (not a directory).
    ///
    /// Directory handles opened via the VFS override this.
    fn getdents64(&mut self, _buf: &mut [u8]) -> i64 {
        crate::fs::types::Errno::ENOTDIR.as_i64()
    }

    /// Name for debugging.
    fn name(&self) -> &str {
        "<unknown>"
    }

    /// Duplicate this handle for inheritance across `fork()`.
    ///
    /// Default `None` means "not inheritable" — matches today's behavior
    /// for device handles (fork only special-cases stdio, see
    /// `FileDescriptorTable::clone`). Handles backed by shared state (e.g.
    /// pipe ends) override this to clone their `Arc` and bump the relevant
    /// refcount, so both parent and child end up sharing the same
    /// underlying buffer — required for pipe semantics across fork.
    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        None
    }

    /// Reposition the file offset. `whence` uses the same values as real
    /// `lseek(2)`: 0 = SEEK_SET, 1 = SEEK_CUR, 2 = SEEK_END. Returns the
    /// new absolute offset.
    ///
    /// Default `NotSupported` — correct for character devices and pipes
    /// (no meaningful position). Regular-file handles (ramfs, initramfs,
    /// ext2) override this.
    fn seek(&mut self, _offset: i64, _whence: i32) -> FileResult<i64> {
        Err(FileError::NotSupported)
    }
}

// ============================================================================
// FILE DESCRIPTOR TABLE
// ============================================================================

const MAX_FILES: usize = 16;

/// Per-process table of open file descriptors.
pub struct FileDescriptorTable {
    files: [Option<Box<dyn FileHandle>>; MAX_FILES],
}

impl FileDescriptorTable {
    /// Create an empty table.
    pub const fn new() -> Self {
        const NONE: Option<Box<dyn FileHandle>> = None;
        Self {
            files: [NONE; MAX_FILES],
        }
    }

    /// Create a table with stdin/stdout/stderr pre-opened.
    /// Uses the driver registry to get default handles.
    pub fn new_with_stdio() -> Self {
        use crate::drivers;

        let mut table = Self::new();

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
        table.files[0] = Some(drivers::open_device("/dev/console")
            .unwrap_or_else(|| Box::new(NullFallback)));

        // FD 1: stdout (framebuffer)
        table.files[1] = Some(drivers::open_device("/dev/fb")
            .unwrap_or_else(|| Box::new(NullFallback)));

        // FD 2: stderr (framebuffer, same as stdout). Used to be bound to
        // `/dev/console` (serial-only) — errors like `ash: clear: not
        // found` were then invisible on the actual screen, only visible by
        // grepping serial.log, since nothing mirrors fb output *back* to
        // serial's own writes. Binding it to `/dev/fb` instead means stderr
        // is on-screen like stdout, and still reaches serial.log too via
        // `framebuffer_console`'s own `mirror_to_serial`.
        table.files[2] = Some(drivers::open_device("/dev/fb")
            .unwrap_or_else(|| Box::new(NullFallback)));

        table
    }

    /// Get a mutable file handle.
    pub fn get_mut(&mut self, fd: usize) -> FileResult<&mut (dyn FileHandle + '_)> {
        if fd >= MAX_FILES {
            return Err(FileError::BadFileDescriptor);
        }

        if let Some(ref mut boxed) = self.files[fd] {
            Ok(&mut **boxed)
        } else {
            Err(FileError::BadFileDescriptor)
        }
    }

    /// Get an immutable file handle.
    pub fn get(&self, fd: usize) -> FileResult<&(dyn FileHandle + '_)> {
        if fd >= MAX_FILES {
            return Err(FileError::BadFileDescriptor);
        }

        self.files[fd]
            .as_ref()
            .map(|boxed| &**boxed)
            .ok_or(FileError::BadFileDescriptor)
    }

    /// Allocate the first free FD for a handle.  Returns the FD number.
    pub fn allocate(&mut self, handle: Box<dyn FileHandle>) -> FileResult<usize> {
        for (i, slot) in self.files.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(handle);
                return Ok(i);
            }
        }

        Err(FileError::InvalidArgument) // Too many files open
    }

    /// dup(2): install a clone of `fd`'s handle at the first free slot
    /// `>= min_fd`. Relies on `FileHandle::dup()` — fds backed by a handle
    /// that doesn't implement it (returns `None`) can't be dup'd; today
    /// that's only directory handles (opendir), which nothing needs to
    /// dup in practice.
    pub fn dup(&mut self, fd: usize, min_fd: usize) -> FileResult<usize> {
        let cloned = self.get(fd)?.dup().ok_or(FileError::NotSupported)?;

        for i in min_fd..MAX_FILES {
            if self.files[i].is_none() {
                self.files[i] = Some(cloned);
                return Ok(i);
            }
        }
        Err(FileError::InvalidArgument) // no free fd
    }

    /// dup2(2): install a clone of `oldfd`'s handle at exactly `newfd`,
    /// closing whatever was already there first. `oldfd == newfd` is a
    /// POSIX-mandated no-op (returns `newfd` without touching anything),
    /// as long as `oldfd` is actually open.
    pub fn dup2(&mut self, oldfd: usize, newfd: usize) -> FileResult<usize> {
        if newfd >= MAX_FILES {
            return Err(FileError::BadFileDescriptor);
        }
        if oldfd == newfd {
            self.get(oldfd)?; // still must be a valid open fd
            return Ok(newfd);
        }

        let cloned = self.get(oldfd)?.dup().ok_or(FileError::NotSupported)?;

        if let Some(mut old) = self.files[newfd].take() {
            let _ = old.close();
        }
        self.files[newfd] = Some(cloned);
        Ok(newfd)
    }

    /// Close a file descriptor.
    pub fn close(&mut self, fd: usize) -> FileResult<()> {
        if fd >= MAX_FILES {
            return Err(FileError::BadFileDescriptor);
        }

        if let Some(mut handle) = self.files[fd].take() {
            handle.close()?;
        }

        Ok(())
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

        if self.files[0].is_some() {
            new_table.files[0] = self.files[0].as_ref().unwrap().dup()
                .or_else(|| crate::drivers::open_device("/dev/console"));
        }
        if self.files[1].is_some() {
            new_table.files[1] = self.files[1].as_ref().unwrap().dup()
                .or_else(|| crate::drivers::open_device("/dev/fb"));
        }
        if self.files[2].is_some() {
            new_table.files[2] = self.files[2].as_ref().unwrap().dup()
                .or_else(|| crate::drivers::open_device("/dev/fb"));
        }

        for i in 3..MAX_FILES {
            if let Some(ref handle) = self.files[i] {
                new_table.files[i] = handle.dup();
            }
        }

        new_table
    }
}