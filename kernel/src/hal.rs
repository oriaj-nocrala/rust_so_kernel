// kernel/src/hal.rs
//
// Kernel-side glue for the `hal` crate's hardware-access seam
// (`hal::PortIo` / `hal::PhysMem`): the actual `x86_64`-backed
// implementations of those traits, plus a minimal `Driver` trait + best-
// effort init registry.
//
// `hal` itself must stay buildable on the host (so its pure logic — the
// ACPI parser today — can be unit tested with `cargo test`), so it cannot
// depend on `x86_64` or touch real hardware. Everything in *this* file is
// the opposite: it only makes sense wired to real ports/memory, which is
// why it lives in the kernel, not in `hal`.

use hal::PhysMem;

/// Production `hal::PortIo`: thin wrapper around `x86_64::instructions::port::Port`.
///
/// Used by `ac97::Ac97Regs<X86PortIo>` (the first driver migrated onto this
/// seam). Zero-sized and `Copy`/`Clone` — so `Ac97Regs<X86PortIo>` (which
/// derives `Copy` when its `IO` is) can be snapshotted out from behind a
/// `Mutex` cheaply before a blocking poll, mirroring the original driver's
/// "poll outside the lock" discipline.
#[derive(Clone, Copy)]
pub struct X86PortIo;

impl hal::PortIo for X86PortIo {
    fn inb(&self, port: u16) -> u8 {
        unsafe { x86_64::instructions::port::Port::new(port).read() }
    }
    fn outb(&self, port: u16, val: u8) {
        unsafe { x86_64::instructions::port::Port::new(port).write(val) }
    }
    fn inw(&self, port: u16) -> u16 {
        unsafe { x86_64::instructions::port::Port::new(port).read() }
    }
    fn outw(&self, port: u16, val: u16) {
        unsafe { x86_64::instructions::port::Port::new(port).write(val) }
    }
    fn inl(&self, port: u16) -> u32 {
        unsafe { x86_64::instructions::port::Port::new(port).read() }
    }
    fn outl(&self, port: u16, val: u32) {
        unsafe { x86_64::instructions::port::Port::new(port).write(val) }
    }
}

/// Production `hal::PhysMem`: reads through the bootloader's fixed
/// physical-memory mapping (`memory::physical_memory_offset()`) — the same
/// idiom every other driver here uses for physical access (ac97's BDL/ring
/// buffers, the original inline `acpi.rs` implementation this replaces).
pub struct KernelPhysMem;

impl PhysMem for KernelPhysMem {
    fn read(&self, pa: u64, buf: &mut [u8]) {
        let src = (crate::memory::physical_memory_offset() + pa).as_ptr::<u8>();
        // SAFETY: caller (the `hal::acpi` parser) is only ever asked to
        // read addresses that come from the bootloader-supplied RSDP or
        // from table pointers/lengths validated (checksum + bounds) before
        // being followed — identical trust model to the physical memory
        // access the original inline `acpi.rs` implementation used.
        unsafe {
            core::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), buf.len());
        }
    }
}

// ── Driver trait + best-effort init registry ────────────────────────────────

/// Minimal, uniform lifecycle for hardware drivers. Deliberately not a full
/// Linux-style probe/bus/device model — with ~10 drivers total in this
/// kernel that would be premature; this is just enough structure to run
/// `init()` uniformly instead of an ad-hoc list of `crate::x::init()` calls
/// in `init/mod.rs`.
pub trait Driver {
    /// Short, human-readable name, used only for boot-log lines.
    fn name(&self) -> &str;

    /// Best-effort initialization. Must never panic — a driver that can't
    /// find/enable its hardware should return `Err` and let the kernel keep
    /// booting, same convention every optional hardware probe here already
    /// follows (mouse, AC97, RTC).
    fn init(&mut self) -> Result<(), DriverError>;
}

/// Why a driver's `init()` didn't succeed. Intentionally coarse — callers
/// only log this, they don't branch on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverError {
    /// The expected hardware/table wasn't present or usable.
    NotFound,
    /// Present, but rejected by a validity check (checksum, signature,
    /// malformed structure, ...).
    Invalid,
}

/// Runs every driver's `init()` in order, logging name + outcome to serial.
/// Never panics regardless of individual driver outcomes — a failing driver
/// just gets logged and skipped, exactly like the ad-hoc
/// `crate::mouse::init(); crate::ac97::init(); ...` list it's meant to
/// eventually replace.
pub fn run_all(drivers: &mut [&mut dyn Driver]) {
    for drv in drivers.iter_mut() {
        match drv.init() {
            Ok(()) => crate::serial_println!("[hal] driver '{}' init: OK", drv.name()),
            Err(e) => crate::serial_println!("[hal] driver '{}' init: FAILED ({:?})", drv.name(), e),
        }
    }
}
