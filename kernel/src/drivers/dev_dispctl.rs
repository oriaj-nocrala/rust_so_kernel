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
//   board cannot run it, EIO if the training failed (see /proc/gpu). With
//   `gpu=modes` (phase 5.7), `mode WxH@Hz` sets the primary head to that
//   EDID or CVT-RB2 mode (same size only), synchronously: detach, retrain
//   if needed, attach at the new mode (`gpu/modeset.rs`); EINVAL if refused
//   before touching anything, EAGAIN if busy, EIO if it failed after.
//   With `gpu=hdmi` (phase 5.8), `hdmi on` / `hdmi off` light the HP on
//   HDMI with the kernel's own picture, or switch it off (`gpu/hdmi.rs`);
//   EINVAL if already so, EAGAIN if busy, EIO if it failed.
//   `trace reset` restarts the pacing statistics (`gpu/pacing.rs`).
//   `peek <offset>` reads one display register from an allow-list (EINVAL
//   otherwise); the value comes back on the next `read` as a `peek:` line.
//   `gsp pstate`: only the current P-state (`gpu_perf: pstate=Pn`), one RM control.
// - `read`: one status line (the SOR's ARMED control, the core channel's
//   PUT/GET), with `gpu=modes` a second (`mode: ...`), then EOF.

use alloc::boxed::Box;

use crate::fs::types::{Errno, Stat};
use crate::gpu::dplink::{self, TrainError};
use crate::gpu::hdmi::{self, HdmiError};
use crate::gpu::modeset::{self, SetError};
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
        let mut s = supervisor::status();
        s.push_str(&modeset::status());
        s.push_str(&hdmi::status());
        s.push_str(&supervisor::peek_status());
        let n = s.len().min(buf.len());
        buf[..n].copy_from_slice(&s.as_bytes()[..n]);
        self.read_done = true;
        Ok(n)
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        let text = core::str::from_utf8(buf).map(str::trim).map_err(|_| FileError::InvalidArgument)?;
        if text == "trace reset" {
            // restart the pacing statistics (`/proc/kdebug` `gpu_pacing:`), to measure one phase
            crate::gpu::pacing::reset();
            return Ok(buf.len());
        }
        if let Some(off) = text.strip_prefix("peek ") {
            // `peek <offset>` (hex with 0x, or decimal): read one display register (allow-list in `nvgpu::evo::peek_allowed`); the value
            // is on the next read of this device, a `peek:` line. An instrument: no side effects.
            let off = off.trim();
            let off = match off.strip_prefix("0x") {
                Some(h) => u32::from_str_radix(h, 16),
                None => off.parse(),
            }
            .map_err(|_| FileError::InvalidArgument)?;
            return supervisor::peek(off).map(|_| buf.len()).ok_or(FileError::InvalidArgument);
        }
        if let Some(req) = text.strip_prefix("mode ") {
            return match modeset::set(req.trim()) {
                Ok(()) => Ok(buf.len()),
                Err(SetError::Busy) => Err(FileError::Again),
                Err(SetError::Invalid) => Err(FileError::InvalidArgument),
                Err(SetError::NotReady | SetError::Failed) => Err(FileError::IOError),
            };
        }
        if let Some(what) = text.strip_prefix("gsp ") {
            // `gsp name`: a control on RM at run time; `gsp poll`: serve RM's status queue now; `gsp perf`: P-state and clocks from RM (gpu=gsp)
            return match what.trim() {
                "name" => crate::gpu::gsp::runtime_name().map(|_| buf.len()).map_err(|_| FileError::IOError),
                "perf" => crate::gpu::gsp::runtime_perf().map(|_| buf.len()).map_err(|_| FileError::IOError),
                "pstate" => crate::gpu::gsp::runtime_pstate().map(|_| buf.len()).map_err(|_| FileError::IOError),
                "poll" => crate::gpu::gsp::poll_events().map(|_| buf.len()).ok_or(FileError::IOError),
                _ => Err(FileError::InvalidArgument),
            };
        }
        if text == "copy fault" {
            return crate::gpu::copy::selftest_fault().map(|_| buf.len()).map_err(|_| FileError::IOError);
        }
        if let Some(args) = text.strip_prefix("copy ") {
            // `copy irq <runs>`: the copy engine's interrupt against polling, at run time (gpu=copy)
            let runs = match args.trim().strip_prefix("irq") {
                Some(n) if n.trim().is_empty() => 20,
                Some(n) => n.trim().parse().map_err(|_| FileError::InvalidArgument)?,
                None => return Err(FileError::InvalidArgument),
            };
            return crate::gpu::copy::selftest_irq(runs).map(|_| buf.len()).map_err(|_| FileError::IOError);
        }
        if let Some(which) = text.strip_prefix("hdmi ") {
            let res = match which.trim() {
                "on" => hdmi::on(),
                "off" => hdmi::off(),
                _ => return Err(FileError::InvalidArgument),
            };
            return match res {
                Ok(()) => Ok(buf.len()),
                Err(HdmiError::Busy) => Err(FileError::Again),
                Err(HdmiError::State) => Err(FileError::InvalidArgument),
                Err(HdmiError::NotReady | HdmiError::Failed) => Err(FileError::IOError),
            };
        }
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
