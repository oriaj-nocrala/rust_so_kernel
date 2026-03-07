// kernel/src/fs/initramfs.rs
//
// In-memory filesystem backed by ELF binaries embedded at compile time.
//
// All programs from `user_programs::PROGRAMS` are accessible at /bin/<name>.
// Reads are sequential; the cursor is tracked in the open `RamFile` handle.
// Writes return EROFS.  No directory listing yet (added with VFS phase 2).

use alloc::boxed::Box;
use crate::process::{
    file::{FileHandle, FileError, FileResult},
    user_programs::{list_programs, ProgramSource},
};

// ── Public API ───────────────────────────────────────────────────────────────

/// Return the raw bytes of an embedded binary by name (without path prefix).
///
/// Used by `sys_exec` and `fs::read_bytes` to feed the ELF loader directly
/// without going through a FileHandle.
pub fn bytes(name: &str) -> Option<&'static [u8]> {
    for (prog_name, source) in list_programs() {
        if *prog_name == name {
            if let ProgramSource::Elf(b) = source {
                return Some(b);
            }
        }
    }
    None
}

/// Open an embedded binary as a seekable FileHandle.
///
/// Returns `None` if `name` is not found in the registry.
pub fn open(name: &str) -> Option<Box<dyn FileHandle>> {
    let data = bytes(name)?;
    Some(Box::new(RamFile { data, offset: 0 }))
}

// ── RamFile — FileHandle over a static byte slice ───────────────────────────

struct RamFile {
    data: &'static [u8],
    offset: usize,
}

impl FileHandle for RamFile {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        let remaining = &self.data[self.offset..];
        if remaining.is_empty() {
            return Ok(0); // EOF
        }
        let n = buf.len().min(remaining.len());
        buf[..n].copy_from_slice(&remaining[..n]);
        self.offset += n;
        Ok(n)
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::NotSupported) // read-only filesystem
    }

    fn name(&self) -> &str {
        "initramfs"
    }
}
