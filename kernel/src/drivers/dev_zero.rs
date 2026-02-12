// kernel/src/drivers/dev_zero.rs
//
// /dev/zero — reads return infinite zeros, writes are discarded.

use alloc::boxed::Box;
use crate::process::file::{FileHandle, FileResult};

pub struct DevZero;

impl FileHandle for DevZero {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        for byte in buf.iter_mut() {
            *byte = 0;
        }
        Ok(buf.len())
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        Ok(buf.len())
    }

    fn name(&self) -> &str {
        "/dev/zero"
    }
}

pub fn open() -> Box<dyn FileHandle> {
    Box::new(DevZero)
}