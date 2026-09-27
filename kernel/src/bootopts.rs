// kernel/src/bootopts.rs
//
// Boot options (`hal::bootopts`): UEFI gives this kernel no command line,
// so `/mnt/etc/kernel.conf` is one, and `/mnt/autorun/kernel.conf` — put
// there by `scripts/metal-run.sh --kconf`, and gone with the rest of
// `autorun/` after the run — overrides it for one unattended boot.
// Read once, right after `fs::init`; before that every option has its
// default.

use hal::bootopts::{BootOpts, GpuLevel};

use crate::serial_println;

const FILES: [&str; 2] = ["/mnt/etc/kernel.conf", "/mnt/autorun/kernel.conf"];

static OPTS: spin::Once<BootOpts> = spin::Once::new();

pub fn load() {
    OPTS.call_once(|| {
        let mut opts = BootOpts::default();
        for path in FILES {
            let Ok(mut fh) = crate::fs::vfs::open(path, crate::fs::types::OpenFlags::RDONLY) else {
                continue;
            };
            let mut buf = [0u8; 1024];
            let n = fh.read(&mut buf).unwrap_or(0);
            let text = core::str::from_utf8(&buf[..n]).unwrap_or("");
            for bad in opts.merge(text) {
                serial_println!("bootopts: {}: ignoring '{}' (not key=value)", path, bad);
            }
        }
        for (k, v) in opts.pairs() {
            serial_println!("bootopts: {}={}", k, v);
        }
        opts
    });
}

pub fn get(key: &str) -> Option<&'static str> {
    OPTS.get()?.get(key)
}

/// `gpu=` (default `off`). An unknown value is logged and read as `off`.
pub fn gpu_level() -> GpuLevel {
    match get("gpu") {
        None => GpuLevel::Off,
        Some(v) => GpuLevel::parse(v).unwrap_or_else(|| {
            serial_println!("bootopts: unknown gpu={}, using off", v);
            GpuLevel::Off
        }),
    }
}
