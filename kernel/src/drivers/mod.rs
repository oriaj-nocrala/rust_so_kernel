// kernel/src/drivers/mod.rs
//
// Device driver registry.
//
// Each driver registers itself as a (path, constructor) pair.
// `open_device(path)` returns a boxed FileHandle, or None.
//
// This replaces the hardcoded `match path` in sys_open.
// Adding a new device driver = add a module + one line in DEVICES.

pub mod dev_null;
pub mod dev_zero;
pub mod serial_console;
pub mod framebuffer_console;

use alloc::boxed::Box;
use crate::process::file::FileHandle;

/// A device entry: path and constructor function.
struct DeviceEntry {
    path: &'static str,
    open: fn() -> Box<dyn FileHandle>,
}

/// Static device registry.  Order doesn't matter.
/// To add a new device: create the module, add one line here.
static DEVICES: &[DeviceEntry] = &[
    DeviceEntry { path: "/dev/null",    open: dev_null::open },
    DeviceEntry { path: "/dev/zero",    open: dev_zero::open },
    DeviceEntry { path: "/dev/console", open: serial_console::open },
    DeviceEntry { path: "/dev/fb",      open: framebuffer_console::open },
];

/// Open a device by path.  Returns `None` if no driver matches.
pub fn open_device(path: &str) -> Option<Box<dyn FileHandle>> {
    DEVICES
        .iter()
        .find(|d| d.path == path)
        .map(|d| (d.open)())
}