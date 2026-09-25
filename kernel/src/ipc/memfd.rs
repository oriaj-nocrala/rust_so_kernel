// kernel/src/ipc/memfd.rs
//
// `memfd_create(2)`'s file: a `FileHandle` over a `memory::shm::ShmObject`.
// `mmap(MAP_SHARED)` reaches the object through `FileHandle::shm_object`
// (see `sys_mmap`); `read`/`write`/`lseek`/`fstat` work on it as on a
// regular file; `ftruncate` sets its size. Passing the fd with
// `SCM_RIGHTS` or `dup` hands over the same object — the way a GUI client
// gives the compositor its buffer (`docs/gui/gui-plan.md`).

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::fs::types::Stat;
use crate::memory::shm::{ShmError, ShmObject};
use crate::process::file::{compute_seek, FileError, FileHandle, FileResult};

/// Inode numbers for `fstat`, distinct per object; above anything a real
/// filesystem here hands out.
static NEXT_INO: AtomicU64 = AtomicU64::new(1 << 40);

#[derive(Clone)]
pub struct MemfdHandle {
    obj: Arc<ShmObject>,
    /// The file offset, shared by every `dup` of this open file
    /// description, as POSIX requires.
    pos: Arc<AtomicU64>,
    ino: u64,
}

impl MemfdHandle {
    pub fn new() -> Self {
        Self {
            obj: Arc::new(ShmObject::new()),
            pos: Arc::new(AtomicU64::new(0)),
            ino: NEXT_INO.fetch_add(1, Ordering::Relaxed),
        }
    }
}

fn file_error(e: ShmError) -> FileError {
    match e {
        // Linux says EFBIG / ENOMEM; `FileError` has no such variants, and
        // "no space left" is what a writer can act on.
        ShmError::TooBig | ShmError::NoMemory => FileError::NoSpace,
        ShmError::Busy => FileError::InvalidArgument,
    }
}

impl FileHandle for MemfdHandle {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        let pos = self.pos.load(Ordering::Acquire);
        let n = self.obj.read_at(pos, buf);
        self.pos.store(pos + n as u64, Ordering::Release);
        Ok(n)
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        let pos = self.pos.load(Ordering::Acquire);
        let n = self.obj.write_at(pos, buf).map_err(file_error)?;
        self.pos.store(pos + n as u64, Ordering::Release);
        Ok(n)
    }

    fn stat(&self) -> Option<Stat> {
        Some(Stat::regular_writable(self.ino, self.obj.size() as i64))
    }

    fn name(&self) -> &str {
        "memfd"
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        Some(Box::new(self.clone()))
    }

    fn seek(&mut self, offset: i64, whence: i32) -> FileResult<i64> {
        let cur = self.pos.load(Ordering::Acquire) as i64;
        let new = compute_seek(cur, self.obj.size() as i64, offset, whence)?;
        self.pos.store(new as u64, Ordering::Release);
        Ok(new)
    }

    fn shm_object(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        Some(self.obj.clone())
    }
}
