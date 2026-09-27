// kernel/src/drivers/dev_dispctl.rs
//
// /dev/dispctl — asks the GPU's display for a change that runs the
// supervisors (phase 5.4 of docs/gpu/gpu-plan.md; `gpu/supervisor.rs`).
//
// - `open`: ENODEV unless `gpu=super` set the supervisors up and vblank's
//   MSI (which carries them) is armed.
// - `write`: `detach` takes the primary head's SOR off (the screen goes
//   dark), `attach` puts the GOP's SOR control back (same mode, same link).
//   One core push each, not waited on: EAGAIN while the previous one is
//   still being worked through, EINVAL for anything else. With `gpu=vpll`
//   (phase 5.5), `clock <kHz>` sets the primary head's pixel clock on the
//   same raster (`clock 0` = the GOP's); EINVAL out of bounds. With
//   `gpu=dplink` (phase 5.6), `train <lanes> <rate>` (rate in DPCD units,
//   e.g. `train 4 0x14` = 4x HBR2) retrains the DP link, synchronously
//   (~130 ms): EAGAIN unless detached and idle, EINVAL if the sink or the
//   board cannot run it, EIO if the training failed (see /proc/gpu).
// - `read`: one status line (the SOR's ARMED control, the core channel's
//   PUT/GET), then EOF.

use alloc::boxed::Box;

use crate::fs::types::{Errno, Stat};
use crate::gpu::dplink::{self, TrainError};
use crate::gpu::supervisor::{self, Cmd, RequestError};
use crate::process::file::{FileError, FileHandle, FileResult};

pub struct DispctlDevice {
    read_done: bool,
}

impl FileHandle for DispctlDevice {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        if self.read_done {
            return Ok(0);
        }
        let s = supervisor::status();
        let n = s.len().min(buf.len());
        buf[..n].copy_from_slice(&s.as_bytes()[..n]);
        self.read_done = true;
        Ok(n)
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        let text = core::str::from_utf8(buf).map(str::trim).map_err(|_| FileError::InvalidArgument)?;
        if let Some(args) = text.strip_prefix("train ") {
            let mut it = args.split_whitespace();
            let num = |s: Option<&str>| -> Result<u8, FileError> {
                let s = s.ok_or(FileError::InvalidArgument)?;
                match s.strip_prefix("0x") {
                    Some(h) => u8::from_str_radix(h, 16),
                    None => s.parse(),
                }
                .map_err(|_| FileError::InvalidArgument)
            };
            let (nr, bw) = (num(it.next())?, num(it.next())?);
            return match dplink::train(nr, bw) {
                Ok(()) => Ok(buf.len()),
                Err(TrainError::Busy) => Err(FileError::Again),
                Err(TrainError::Refused) => Err(FileError::InvalidArgument),
                Err(TrainError::NotReady | TrainError::Failed) => Err(FileError::IOError),
            };
        }
        let cmd = match text.split_once(' ') {
            None if text == "detach" => Cmd::Detach,
            None if text == "attach" => Cmd::Attach,
            Some(("clock", khz)) => Cmd::Clock(khz.trim().parse().map_err(|_| FileError::InvalidArgument)?),
            _ => return Err(FileError::InvalidArgument),
        };
        match supervisor::request(cmd) {
            Ok(_) => Ok(buf.len()),
            Err(RequestError::Busy) => Err(FileError::Again),
            Err(RequestError::Invalid) => Err(FileError::InvalidArgument),
            Err(RequestError::NotReady | RequestError::Chan(_)) => Err(FileError::IOError),
        }
    }

    fn stat(&self) -> Option<Stat> {
        Some(Stat::chardev(0))
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        Some(Box::new(DispctlDevice { read_done: self.read_done }))
    }

    fn name(&self) -> &str {
        "/dev/dispctl"
    }
}

pub fn open() -> Result<Box<dyn FileHandle>, Errno> {
    if !supervisor::ready() {
        return Err(Errno::ENODEV);
    }
    Ok(Box::new(DispctlDevice { read_done: false }))
}
