// kernel/src/drivers/dev_random.rs
//
// /dev/urandom and /dev/random — reads return random bytes (`crate::random`), writes are accepted and mixed in (as on Linux, a
// write feeds the pool without crediting entropy).

use alloc::boxed::Box;
use crate::fs::types::Stat;
use crate::process::file::{FileHandle, FileResult};

pub struct DevRandom {
    name: &'static str,
}

impl FileHandle for DevRandom {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        crate::random::fill(buf);
        Ok(buf.len())
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        crate::random::add_entropy(buf);
        Ok(buf.len())
    }

    fn stat(&self) -> Option<Stat> {
        // major 1, minor 8 (random) / 9 (urandom), as on Linux
        Some(Stat::chardev(if self.name == "/dev/random" { 0x0108 } else { 0x0109 }))
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        Some(Box::new(DevRandom { name: self.name }))
    }

    fn name(&self) -> &str {
        self.name
    }
}

pub fn open(name: &'static str) -> Box<dyn FileHandle> {
    Box::new(DevRandom { name })
}
