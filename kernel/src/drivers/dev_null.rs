// kernel/src/drivers/dev_null.rs
//
// /dev/null — discards all writes, reads return EOF.

use alloc::boxed::Box;
use crate::fs::types::Stat;
use crate::process::file::{FileHandle, FileResult};

pub struct DevNull;

impl FileHandle for DevNull {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        Ok(0) // EOF
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        Ok(buf.len()) // Pretend everything was written
    }

    fn stat(&self) -> Option<Stat> {
        Some(Stat::chardev(0))
    }

    // Stateless (no fields at all) — a fresh instance behaves identically
    // to the original, so "duplicating" it is just making another one.
    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        Some(Box::new(DevNull))
    }

    fn name(&self) -> &str {
        "/dev/null"
    }
}

pub fn open() -> Box<dyn FileHandle> {
    Box::new(DevNull)
}