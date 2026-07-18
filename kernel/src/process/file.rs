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
    /// The operation would block (empty pipe on read, full pipe on write).
    /// `sys_read`/`sys_write` catch this, drop the fd-table lock, and
    /// perform the actual block_current/jump_to_trapframe themselves — see
    /// their doc comments for why this can't happen inside `read`/`write`.
    WouldBlock,
}

pub type FileResult<T> = Result<T, FileError>;

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

        // FD 0: stdin (for now, /dev/null)
        table.files[0] = Some(drivers::open_device("/dev/null")
            .unwrap_or_else(|| Box::new(NullFallback)));

        // FD 1: stdout (framebuffer)
        table.files[1] = Some(drivers::open_device("/dev/fb")
            .unwrap_or_else(|| Box::new(NullFallback)));

        // FD 2: stderr (serial console)
        table.files[2] = Some(drivers::open_device("/dev/console")
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

// fds 0-2 get fresh stdio handles (same as before); fds 3+ are inherited via
// `dup()` when the underlying handle supports it (e.g. pipe ends) — needed
// so a pipe created before `fork()` is usable by both parent and child.
impl Clone for FileDescriptorTable {
    fn clone(&self) -> Self {
        let mut new_table = Self::new();

        if self.files[0].is_some() {
            new_table.files[0] = crate::drivers::open_device("/dev/null");
        }
        if self.files[1].is_some() {
            new_table.files[1] = crate::drivers::open_device("/dev/fb");
        }
        if self.files[2].is_some() {
            new_table.files[2] = crate::drivers::open_device("/dev/console");
        }

        for i in 3..MAX_FILES {
            if let Some(ref handle) = self.files[i] {
                new_table.files[i] = handle.dup();
            }
        }

        new_table
    }
}