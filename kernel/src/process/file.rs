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
}

pub type FileResult<T> = Result<T, FileError>;

// ============================================================================
// TRAIT: FileHandle
// ============================================================================

/// Trait representing any "file" in the system.
///
/// Implementations include device drivers (/dev/null, /dev/console, etc.),
/// and in the future: pipes, sockets, real files on disk.
pub trait FileHandle: Send {
    /// Read up to `buf.len()` bytes.  Returns bytes read.
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize>;

    /// Write up to `buf.len()` bytes.  Returns bytes written.
    fn write(&mut self, buf: &[u8]) -> FileResult<usize>;

    /// Close the file (optional, default no-op).
    fn close(&mut self) -> FileResult<()> {
        Ok(())
    }

    /// Name for debugging.
    fn name(&self) -> &str {
        "<unknown>"
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

        // FD 1: stdout (serial console)
        table.files[1] = Some(drivers::open_device("/dev/console")
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

// Clone creates fresh stdio handles (same as fork would)
impl Clone for FileDescriptorTable {
    fn clone(&self) -> Self {
        let mut new_table = Self::new();

        if self.files[0].is_some() {
            new_table.files[0] = crate::drivers::open_device("/dev/null");
        }
        if self.files[1].is_some() {
            new_table.files[1] = crate::drivers::open_device("/dev/console");
        }
        if self.files[2].is_some() {
            new_table.files[2] = crate::drivers::open_device("/dev/console");
        }

        new_table
    }
}