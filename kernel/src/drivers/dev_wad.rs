// kernel/src/drivers/dev_wad.rs
//
// /dev/freedoom1.wad — the Freedoom Phase 1 IWAD, embedded in the kernel
// image and served as a seekable read-only "file" (same handle shape as
// initramfs::RamFile: static byte slice + Arc'd offset).
//
// Why a device and not /mnt/freedoom1.wad (ext2): DOOM's access pattern
// (fopen + SEEK_END size probe + scattered lump reads) hit transient ATA
// read corruption — after one bad PIO transaction the whole channel
// returned empty reads for the rest of the boot, and block/ata.rs has no
// error-recovery/reset path. Kernel memory has no such failure mode.
//
// Why this exact name and not "wad0": doomgeneric validates the *path
// string* itself. w_wad.c::W_AddFile treats any file whose name doesn't
// end in "wad" as a single loose lump (strcasecmp on the last 3 chars),
// and d_iwad.c::IdentifyIWADByName must match the basename against its
// iwads[] table ("freedoom1.wad" → retail doom) or D_DoomMain aborts
// with "Unknown or invalid IWAD file". Naming the device exactly like
// the real file satisfies both checks without patching the submodule.

use alloc::boxed::Box;
use alloc::sync::Arc;
use spin::Mutex;

use crate::process::file::{compute_seek, FileError, FileHandle, FileResult};

static WAD: &[u8] = include_bytes!("../../embedded/freedoom1.wad");

pub fn open() -> Box<dyn FileHandle> {
    Box::new(WadHandle { offset: Arc::new(Mutex::new(0)) })
}

/// Seekable read-only handle over the embedded WAD.
struct WadHandle {
    // Arc'd so dup()/dup2() share one "open file description" position
    // (POSIX dup() semantics) — same reasoning as initramfs::RamFile.
    offset: Arc<Mutex<usize>>,
}

impl FileHandle for WadHandle {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        let mut offset = self.offset.lock();
        // seek() past EOF is legal — treat any out-of-range offset as EOF.
        let remaining = WAD.get(*offset..).unwrap_or(&[]);
        if remaining.is_empty() {
            return Ok(0); // EOF
        }
        let n = buf.len().min(remaining.len());
        buf[..n].copy_from_slice(&remaining[..n]);
        *offset += n;
        Ok(n)
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::NotSupported)
    }

    fn stat(&self) -> Option<crate::fs::types::Stat> {
        let ino = crate::drivers::device_index("/dev/freedoom1.wad").unwrap_or(0) as u64;
        Some(crate::fs::types::Stat::regular(ino, WAD.len() as i64))
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        Some(Box::new(WadHandle { offset: self.offset.clone() }))
    }

    fn seek(&mut self, offset: i64, whence: i32) -> FileResult<i64> {
        let mut cur = self.offset.lock();
        let new_pos = compute_seek(*cur as i64, WAD.len() as i64, offset, whence)?;
        *cur = new_pos as usize;
        Ok(new_pos)
    }

    fn name(&self) -> &str {
        "/dev/freedoom1.wad"
    }
}
