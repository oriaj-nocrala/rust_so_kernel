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