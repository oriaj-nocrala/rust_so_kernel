// kernel/src/drivers/mod.rs
//
// Device driver registry.
//
// Each driver registers itself as a (path, constructor) pair.
// `open_device(path)` returns a boxed FileHandle, or None.
//
// This replaces the hardcoded `match path` in sys_open.
// Adding a new device driver = add a module + one line in DEVICES.

pub(crate) mod evdev;
pub mod dev_dsp;
pub mod dev_fb0;
pub mod dev_input_event;
pub mod dev_kbd;
pub mod dev_mouse_event;
pub mod dev_null;
pub mod dev_zero;
pub mod serial_console;
pub mod framebuffer_console;

use alloc::boxed::Box;
use crate::process::file::FileHandle;
use crate::fs::types::Errno;

/// A device entry: path and constructor function.
struct DeviceEntry {
    path: &'static str,
    /// Most devices always open; one that is exclusive or depends on
    /// hardware (`/dev/fb0`) says why it did not.
    open: fn() -> Result<Box<dyn FileHandle>, Errno>,
}

/// Static device registry.  Order doesn't matter.
/// To add a new device: create the module, add one line here.
static DEVICES: &[DeviceEntry] = &[
    DeviceEntry { path: "/dev/kbd",     open: || Ok(dev_kbd::open()) },
    DeviceEntry { path: "/dev/null",    open: || Ok(dev_null::open()) },
    DeviceEntry { path: "/dev/zero",    open: || Ok(dev_zero::open()) },
    DeviceEntry { path: "/dev/console", open: || Ok(serial_console::open()) },
    DeviceEntry { path: "/dev/fb",      open: || Ok(framebuffer_console::open()) },
    // The compositor's screen: exclusive, and open = graphics mode.
    DeviceEntry { path: "/dev/fb0",     open: dev_fb0::open },
    // Nested under /dev/input/, same layout real Linux uses for evdev
    // devices — see fs/devfs.rs's InputDirInode for the one-level
    // subdirectory support this needs (devfs is otherwise flat).
    DeviceEntry { path: "/dev/input/event0", open: || Ok(dev_input_event::open()) }, // keyboard
    DeviceEntry { path: "/dev/input/event1", open: || Ok(dev_mouse_event::open()) }, // mouse
    DeviceEntry { path: "/dev/dsp", open: || Ok(dev_dsp::open()) }, // AC97 PCM output, see ac97.rs
    // Pseudo-terminals (ipc/pty.rs): each open of ptmx is a new pair; the
    // slaves are /dev/pts/<n> (fs/devfs.rs's PtsDirInode). /dev/tty is the
    // caller's controlling terminal (ENXIO without one).
    DeviceEntry { path: "/dev/ptmx", open: crate::ipc::pty::open_master },
    DeviceEntry { path: "/dev/tty",  open: crate::ipc::pty::open_controlling },
];

/// Open a device by path.  Returns `None` if no driver matches.
pub fn open_device(path: &str) -> Result<Box<dyn FileHandle>, Errno> {
    DEVICES
        .iter()
        .find(|d| d.path == path)
        .ok_or(Errno::ENOENT)
        .and_then(|d| (d.open)())
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