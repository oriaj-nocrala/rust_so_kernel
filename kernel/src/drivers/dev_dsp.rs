// kernel/src/drivers/dev_dsp.rs
//
// /dev/dsp — write-only PCM output device, OSS's classic device name and
// write()-raw-samples convention (real OSS also supports SNDCTL_DSP_*
// ioctls to negotiate format/rate; skipped here since there's exactly one
// client (DOOM's sound module) and one hardware format — see ac97.rs's
// module doc). Fixed format: 48000 Hz, stereo, signed 16-bit little-endian
// interleaved PCM, matching AC97's native non-VRA operating point exactly.
//
// A write() blocks (via ac97::write_pcm's internal spin) until hardware
// buffer space is available, then returns however many bytes it actually
// accepted — same "may write less than requested" contract a real
// blocking OSS device has, so callers must loop until all bytes are sent.

use alloc::boxed::Box;
use crate::fs::types::Stat;
use crate::process::file::{FileError, FileHandle, FileResult};

pub struct DspDevice;

impl FileHandle for DspDevice {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        Err(FileError::NotSupported) // output-only
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        Ok(crate::ac97::write_pcm(buf))
    }

    fn stat(&self) -> Option<Stat> {
        Some(Stat::chardev(0))
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        Some(Box::new(DspDevice))
    }

    fn name(&self) -> &str {
        "/dev/dsp"
    }
}

pub fn open() -> Box<dyn FileHandle> {
    Box::new(DspDevice)
}
