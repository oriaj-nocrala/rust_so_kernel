//! `hal` — hardware-access seam for the kernel.
//!
//! This crate holds the pure, hardware-agnostic logic that used to live
//! directly inside kernel driver modules, plus the traits ("seams") that let
//! that logic be exercised on the host with `cargo test` instead of only
//! inside QEMU.
//!
//! `#![no_std]` except under `cfg(test)`, where the test harness itself
//! needs `std` — this is the standard idiom for a `no_std` crate that also
//! wants host-side unit tests. Either way it links `alloc` (for `Vec`),
//! which is available both on the bare-metal kernel target (via
//! `-Z build-std`) and on the host (as a normal sysroot component).
//!
//! Crucially, `hal` has **zero bare-metal-only dependencies** — no `x86_64`
//! crate, no raw port I/O, nothing that only makes sense with real hardware
//! behind it. The kernel provides the real (`x86_64`-backed) implementations
//! of the traits below; this crate only defines the traits, a couple of
//! simple mocks, and pure parsing logic generic over them.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod acpi;
pub mod ac97;
pub mod block;
pub mod keyboard;
pub mod mouse;
pub mod pit;
pub mod rtc;

/// Legacy x86 port I/O seam. The production implementation (kernel side)
/// wraps `x86_64::instructions::port::Port`; tests back it with `MockIo`
/// (last-write-wins) or `ScriptedIo` (scriptable reads, recorded writes).
pub trait PortIo {
    fn inb(&self, port: u16) -> u8;
    fn outb(&self, port: u16, val: u8);
    fn inw(&self, port: u16) -> u16;
    fn outw(&self, port: u16, val: u16);
    fn inl(&self, port: u16) -> u32;
    fn outl(&self, port: u16, val: u32);
}

/// Blanket impl so a driver's register-protocol struct (`Ac97Regs<IO>` and
/// friends) can be generic over `IO: PortIo` while a test holds onto its
/// mock (e.g. `ScriptedIo`) by value and passes a `&mock` — the reference
/// itself is `PortIo`, and being a plain reference it's always `Copy`,
/// which lets those protocol structs derive `Copy`/`Clone` for cheap
/// snapshotting (see `hal::ac97::Ac97Regs`'s doc comment for why that
/// matters: snapshotting out from behind a lock before a blocking poll).
impl<T: PortIo + ?Sized> PortIo for &T {
    fn inb(&self, port: u16) -> u8 {
        (**self).inb(port)
    }
    fn outb(&self, port: u16, val: u8) {
        (**self).outb(port, val)
    }
    fn inw(&self, port: u16) -> u16 {
        (**self).inw(port)
    }
    fn outw(&self, port: u16, val: u16) {
        (**self).outw(port, val)
    }
    fn inl(&self, port: u16) -> u32 {
        (**self).inl(port)
    }
    fn outl(&self, port: u16, val: u32) {
        (**self).outl(port, val)
    }
}

/// Physical-memory read seam — lets pure parsing logic (ACPI tables today,
/// potentially others later) read physical memory without knowing HOW that
/// memory is mapped. The production implementation (kernel side) reads
/// through the bootloader's fixed physical-memory offset; tests back it
/// with a plain in-memory buffer.
pub trait PhysMem {
    fn read(&self, pa: u64, buf: &mut [u8]);
}

/// A trivial `PortIo` backed by an in-memory register map — good enough for
/// unit tests of logic that reads/writes ports, without needing real
/// hardware or QEMU. Not currently used by the ACPI parser (which only
/// needs `PhysMem`), but kept here so the port-I/O seam has a ready-made
/// mock for the next driver that migrates onto this pattern (ac97, mouse,
/// etc. — see the HAL refactor plan).
pub struct MockIo {
    regs: spin::Mutex<alloc::collections::BTreeMap<u16, u32>>,
}

impl MockIo {
    pub fn new() -> Self {
        MockIo { regs: spin::Mutex::new(alloc::collections::BTreeMap::new()) }
    }
}

impl Default for MockIo {
    fn default() -> Self {
        Self::new()
    }
}

impl PortIo for MockIo {
    fn inb(&self, port: u16) -> u8 {
        (*self.regs.lock().get(&port).unwrap_or(&0)) as u8
    }
    fn outb(&self, port: u16, val: u8) {
        self.regs.lock().insert(port, val as u32);
    }
    fn inw(&self, port: u16) -> u16 {
        (*self.regs.lock().get(&port).unwrap_or(&0)) as u16
    }
    fn outw(&self, port: u16, val: u16) {
        self.regs.lock().insert(port, val as u32);
    }
    fn inl(&self, port: u16) -> u32 {
        *self.regs.lock().get(&port).unwrap_or(&0)
    }
    fn outl(&self, port: u16, val: u32) {
        self.regs.lock().insert(port, val);
    }
}

/// A scriptable `PortIo` mock for tests that need more than `MockIo`'s
/// last-write-wins register map — specifically, driving a status-register
/// poll loop through "not ready" N times before "ready", and asserting the
/// exact sequence of writes a register-protocol method issued (not just
/// their final state).
///
/// Per-port reads are served from a FIFO queue (`queue_read`/
/// `queue_reads`); once a port's queue runs dry, reads keep returning the
/// last value that was ever dequeued for it (or 0 if none was), so a test
/// only needs to script the *transition* ("not ready" a few times, then
/// "ready") and can let a poll loop keep re-reading "ready" afterwards
/// without re-queueing it forever. Every write is appended to an ordered
/// log, inspectable via `writes()`.
///
/// `PortIo`'s methods take `&self` (so a single instance can be shared
/// behind a plain reference the way `Ac97Regs<&ScriptedIo>` does — see the
/// blanket `impl<T: PortIo> PortIo for &T` above), so the mutable state
/// here needs interior mutability; `RefCell` is enough since this is
/// single-threaded host test code (unlike `MockIo`, which uses a real
/// `spin::Mutex` because it's compiled into the no_std side too).
#[cfg(test)]
pub struct ScriptedIo {
    reads: core::cell::RefCell<alloc::collections::BTreeMap<u16, alloc::collections::VecDeque<u32>>>,
    sticky: core::cell::RefCell<alloc::collections::BTreeMap<u16, u32>>,
    writes: core::cell::RefCell<alloc::vec::Vec<(u16, u32)>>,
}

#[cfg(test)]
impl ScriptedIo {
    pub fn new() -> Self {
        ScriptedIo {
            reads: core::cell::RefCell::new(alloc::collections::BTreeMap::new()),
            sticky: core::cell::RefCell::new(alloc::collections::BTreeMap::new()),
            writes: core::cell::RefCell::new(alloc::vec::Vec::new()),
        }
    }

    /// Enqueues one value to be returned by the next read of `port`.
    pub fn queue_read(&self, port: u16, val: u32) {
        self.reads.borrow_mut().entry(port).or_default().push_back(val);
    }

    /// Enqueues several values, in order, to be returned by successive
    /// reads of `port`.
    pub fn queue_reads(&self, port: u16, vals: &[u32]) {
        for &v in vals {
            self.queue_read(port, v);
        }
    }

    /// The full, ordered sequence of `(port, value)` writes issued so far
    /// (value widened to `u32` regardless of whether it came through
    /// `outb`/`outw`/`outl` — enough to assert offsets and payloads without
    /// caring about the access width).
    pub fn writes(&self) -> alloc::vec::Vec<(u16, u32)> {
        self.writes.borrow().clone()
    }

    fn read(&self, port: u16) -> u32 {
        if let Some(v) = self.reads.borrow_mut().get_mut(&port).and_then(|q| q.pop_front()) {
            self.sticky.borrow_mut().insert(port, v);
            return v;
        }
        *self.sticky.borrow().get(&port).unwrap_or(&0)
    }

    fn write(&self, port: u16, val: u32) {
        self.writes.borrow_mut().push((port, val));
    }
}

#[cfg(test)]
impl Default for ScriptedIo {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl PortIo for ScriptedIo {
    fn inb(&self, port: u16) -> u8 {
        self.read(port) as u8
    }
    fn outb(&self, port: u16, val: u8) {
        self.write(port, val as u32);
    }
    fn inw(&self, port: u16) -> u16 {
        self.read(port) as u16
    }
    fn outw(&self, port: u16, val: u16) {
        self.write(port, val as u32);
    }
    fn inl(&self, port: u16) -> u32 {
        self.read(port)
    }
    fn outl(&self, port: u16, val: u32) {
        self.write(port, val);
    }
}
