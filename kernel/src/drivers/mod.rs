// kernel/src/drivers/mod.rs
//
// Device driver registry.
//
// Each driver registers itself as a (path, constructor) pair.
// `open_device(path)` returns a boxed FileHandle, or None.
//
// This replaces the hardcoded `match path` in sys_open.
// Adding a new device driver = add a module + one line in DEVICES.

pub mod dev_kbd;
pub mod dev_kbdraw;
pub mod dev_null;
pub mod dev_wad;
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
    DeviceEntry { path: "/dev/kbd",     open: dev_kbd::open },
    DeviceEntry { path: "/dev/kbdraw",  open: dev_kbdraw::open },
    DeviceEntry { path: "/dev/null",    open: dev_null::open },
    DeviceEntry { path: "/dev/zero",    open: dev_zero::open },
    DeviceEntry { path: "/dev/console", open: serial_console::open },
    DeviceEntry { path: "/dev/fb",      open: framebuffer_console::open },
    // Named after the real file, not "wad0" — doomgeneric validates the
    // path string itself (extension + basename), see dev_wad.rs.
    DeviceEntry { path: "/dev/freedoom1.wad", open: dev_wad::open },
];

/// Open a device by path.  Returns `None` if no driver matches.
pub fn open_device(path: &str) -> Option<Box<dyn FileHandle>> {
    DEVICES
        .iter()
        .find(|d| d.path == path)
        .map(|d| (d.open)())
}

/// Check if a device path is registered.
pub fn has_device(path: &str) -> bool {
    DEVICES.iter().any(|d| d.path == path)
}

/// Return the index of a device in the registry, for stable inode numbers.
pub fn device_index(path: &str) -> Option<usize> {
    DEVICES.iter().position(|d| d.path == path)
}

/// Return the path of the device at `index`, for `readdir`.
pub fn device_by_index(index: usize) -> Option<&'static str> {
    DEVICES.get(index).map(|d| d.path)
}