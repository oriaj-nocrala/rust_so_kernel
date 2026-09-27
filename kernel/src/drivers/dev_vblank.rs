// kernel/src/drivers/dev_vblank.rs
//
// /dev/vblank — the GPU's vblank (phase 3 of docs/gpu/gpu-plan.md), for the
// compositor to present on.
//
// - `open`: ENODEV unless `gpu=vblank` armed the interrupt.
// - `poll`/`epoll`: readable once a vblank of the primary head happened
//   after the one this handle last read (a fresh handle has "read" the
//   current one, so its first wait is for the next vblank).
// - `read`: never blocks. Needs 16 bytes: the vblank sequence number and
//   its `CLOCK_MONOTONIC` time in ns (both u64, native endian). Marks that
//   number seen.
//
// Blocking is `poll`'s: the handle reports `QUEUE_VBLANK` with the number
// it has seen, the MSI handler wakes pollers of that queue, and `poll`
// re-checks the live number after registering (check-then-sleep in one
// step, CLAUDE.md).

use alloc::boxed::Box;

use crate::drivers::evdev::QUEUE_VBLANK;
use crate::fs::types::{Errno, Stat};
use crate::process::file::{FileError, FileHandle, FileResult};

pub const RECORD_SIZE: usize = 16;

pub struct VblankDevice {
    seen: u64,
}

impl FileHandle for VblankDevice {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        if buf.len() < RECORD_SIZE {
            return Err(FileError::InvalidArgument);
        }
        // The time is stored before the number (vblank.rs): read the number
        // first, so the time is at least that recent.
        let seq = crate::gpu::vblank::seq();
        let ns = crate::gpu::vblank::last_ns();
        buf[..8].copy_from_slice(&seq.to_ne_bytes());
        buf[8..16].copy_from_slice(&ns.to_ne_bytes());
        self.seen = seq;
        Ok(RECORD_SIZE)
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::NotSupported)
    }

    fn stat(&self) -> Option<Stat> {
        Some(Stat::chardev(0))
    }

    fn event_source(&self) -> Option<vfs::file::EventSource> {
        Some(vfs::file::EventSource { queue: QUEUE_VBLANK, buffered: false, seen: self.seen })
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        Some(Box::new(VblankDevice { seen: self.seen }))
    }

    fn name(&self) -> &str {
        "/dev/vblank"
    }
}

pub fn open() -> Result<Box<dyn FileHandle>, Errno> {
    if !crate::gpu::vblank::armed() {
        return Err(Errno::ENODEV);
    }
    Ok(Box::new(VblankDevice { seen: crate::gpu::vblank::seq() }))
}
