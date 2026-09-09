// vfs/src/file.rs
//
// The `FileHandle` trait and its error types — the coupling point between
// processes, device drivers, and filesystems. Every open file descriptor in
// the kernel (a device, a VFS-opened regular file, a pipe end, a socket)
// is, underneath, a `Box<dyn FileHandle>`.
//
// The per-process `FileDescriptorTable` is NOT here — it stays in
// `kernel/src/process/file.rs`, since it needs `crate::drivers` (the device
// registry) and `crate::serial_println!` (debug logging), neither of which
// this host-testable crate can reach. See `docs/fs/vfs-extraction-plan.md`.

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
    fn stat(&self) -> Option<crate::types::Stat> {
        None
    }

    /// Fill `buf` with `linux_dirent64` records.  Returns bytes written, or a
    /// negative errno on error.  Default returns `-ENOTDIR` (not a directory).
    ///
    /// Directory handles opened via the VFS override this.
    fn getdents64(&mut self, _buf: &mut [u8]) -> i64 {
        crate::types::Errno::ENOTDIR.as_i64()
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

    /// Change this open file's permission bits — `fchmod(2)`. Default
    /// `Ok(())` matches `Inode::chmod`'s same "pre-existing stub behavior"
    /// default (see its doc comment); only `ext2::Ext2FileHandle`
    /// overrides it, since ext2 is the only filesystem here with a real
    /// on-disk mode field to persist the change into.
    fn chmod(&mut self, _mode: u32) -> FileResult<()> {
        Ok(())
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── compute_seek ─────────────────────────────────────────────────────

    #[test]
    fn seek_set_ignores_current_position() {
        // SEEK_SET (whence 0) computes purely from the requested offset —
        // the current position (50 here) must not leak into the result.
        assert_eq!(compute_seek(50, 100, 10, 0), Ok(10));
    }

    #[test]
    fn seek_cur_adds_to_current_position() {
        assert_eq!(compute_seek(50, 100, 10, 1), Ok(60));
    }

    #[test]
    fn seek_end_adds_to_file_size() {
        assert_eq!(compute_seek(50, 100, 10, 2), Ok(110));
    }

    #[test]
    fn seek_negative_offsets_within_bounds() {
        // SEEK_END with a negative offset: seek to 10 bytes before EOF.
        assert_eq!(compute_seek(0, 100, -10, 2), Ok(90));
        // SEEK_CUR with a negative offset: step backward from position 20.
        assert_eq!(compute_seek(20, 100, -5, 1), Ok(15));
    }

    #[test]
    fn seek_before_byte_zero_is_rejected_seek_set() {
        assert_eq!(compute_seek(50, 100, -1, 0), Err(FileError::InvalidArgument));
    }

    #[test]
    fn seek_before_byte_zero_is_rejected_seek_cur() {
        assert_eq!(compute_seek(5, 100, -10, 1), Err(FileError::InvalidArgument));
    }

    #[test]
    fn seek_before_byte_zero_is_rejected_seek_end() {
        assert_eq!(compute_seek(0, 10, -20, 2), Err(FileError::InvalidArgument));
    }

    #[test]
    fn seek_past_eof_is_allowed() {
        // Real lseek(2) permits seeking past the end of the file — the
        // next read() just returns 0, or (on a writable filesystem) a
        // later write() there creates a hole. This is a deliberate,
        // documented decision, not an oversight: fixed here so nobody
        // "corrects" it into an EINVAL/EFBIG check later.
        assert_eq!(compute_seek(0, 10, 1000, 0), Ok(1000));
    }

    #[test]
    fn seek_invalid_whence_is_rejected() {
        // `3` is not a valid whence value. This repo has a real history
        // of ABI mismatches around exactly this constant — an mlibc
        // sysdeps header once defined SEEK_SET as 3 instead of 0 (see
        // CLAUDE.md's mlibc-port section), which this function's
        // rejection of an unrecognized whence would have surfaced
        // immediately had this test existed at the time.
        assert_eq!(compute_seek(0, 100, 0, 3), Err(FileError::InvalidArgument));
        assert_eq!(compute_seek(0, 100, 0, -1), Err(FileError::InvalidArgument));
    }

    #[test]
    fn seek_i64_overflow_is_rejected() {
        assert_eq!(
            compute_seek(1, 100, i64::MAX, 1),
            Err(FileError::InvalidArgument)
        );
    }

    // ── FileHandle trait defaults ───────────────────────────────────────

    /// Minimal handle implementing only the two required methods, to pin
    /// down exactly what every default does.
    struct MinimalHandle;

    impl FileHandle for MinimalHandle {
        fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
            Ok(0)
        }
        fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
            Ok(buf.len())
        }
    }

    #[test]
    fn default_close_is_ok() {
        assert_eq!(MinimalHandle.close(), Ok(()));
    }

    #[test]
    fn default_stat_is_none() {
        assert!(MinimalHandle.stat().is_none());
    }

    #[test]
    fn default_getdents64_is_enotdir() {
        let mut h = MinimalHandle;
        let got = h.getdents64(&mut []);
        assert_eq!(got, crate::types::Errno::ENOTDIR.as_i64());
        // Pinned against the literal too, so this test fails loudly if
        // someone ever changes the value ENOTDIR resolves to.
        assert_eq!(got, -20);
    }

    #[test]
    fn default_name_is_unknown() {
        assert_eq!(MinimalHandle.name(), "<unknown>");
    }

    #[test]
    fn default_dup_is_none() {
        assert!(MinimalHandle.dup().is_none());
    }

    #[test]
    fn default_seek_is_not_supported() {
        let mut h = MinimalHandle;
        assert_eq!(h.seek(0, 0), Err(FileError::NotSupported));
    }

    #[test]
    fn default_chmod_is_ok() {
        let mut h = MinimalHandle;
        assert_eq!(h.chmod(0o644), Ok(()));
    }

    /// A handle that overrides `seek`/`dup`, to prove the defaults tested
    /// above are actually the *trait's* defaults and not just an
    /// incidental pass — if `MinimalHandle`'s results above were true
    /// regardless of what a handle does, these overrides wouldn't change
    /// anything either.
    struct OverridingHandle;

    impl FileHandle for OverridingHandle {
        fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
            Ok(0)
        }
        fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
            Ok(buf.len())
        }
        fn seek(&mut self, offset: i64, _whence: i32) -> FileResult<i64> {
            Ok(offset)
        }
        fn dup(&self) -> Option<Box<dyn FileHandle>> {
            Some(Box::new(OverridingHandle))
        }
    }

    #[test]
    fn overridden_methods_win_over_trait_defaults() {
        let mut h = OverridingHandle;
        assert_eq!(h.seek(42, 0), Ok(42));
        assert!(OverridingHandle.dup().is_some());
    }
}
