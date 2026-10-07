// kernel/src/block/mod.rs
//
// Block device layer. `ata` is the (only, real-hardware) driver;
// `AtaBlockDevice` is the thin `hal::block::BlockDevice` face `fs::ext2`
// mounts against at real boot. `hal::block::MemDisk` (re-exported below) is
// the other implementation of that same trait — a `Vec<u8>`-backed disk
// used by `fs::ext2`'s QEMU integration test (`kernel/src/hw_tests.rs`) to
// exercise the read-write ext2 path without touching real hardware or
// `disk.img`. See `hal/src/block.rs` for why the seam lives there and why
// it speaks in sectors rather than filesystem blocks.
//
// `ata.rs` itself is deliberately NOT migrated onto the `hal::PortIo` seam
// the way acpi/ac97/keyboard/mouse/pit/rtc are (see `docs/drivers/
// architecture.md`) — that's a separate, not-yet-done piece of work; this
// file only adds the `BlockDevice` seam *above* it, unchanged underneath.

pub mod ata;
pub mod logpart;
pub mod usb;

pub use hal::block::{BlockDevice, Partition, SECTOR_SIZE};

// `MemDisk` (the `Vec<u8>`-backed test double) is only ever constructed from
// `kernel/src/hw_tests.rs`, which is itself `#[cfg(test)]`-only — gating
// the re-export the same way keeps a normal `cargo build` free of an
// "unused import" warning instead of pretending real boot code needs it.
#[cfg(test)]
pub use hal::block::MemDisk;

/// Kernel-side `BlockDevice` seam for the real ATA disk — the implementation
/// `fs::ext2::init()` mounts against at real boot.
///
/// Zero-sized: `block::ata`'s own module-level state (`ATA_LOCK`) already
/// owns the hardware channel, so this is just a `BlockDevice` face on top of
/// the free functions in `block::ata` (untouched by this seam — see the
/// module doc comment above).
#[derive(Clone, Copy, Default)]
pub struct AtaBlockDevice;

impl BlockDevice for AtaBlockDevice {
    fn present(&self) -> bool {
        ata::present()
    }

    fn read_sectors(&self, lba: u32, count: u8, buf: &mut [u8]) -> Result<(), &'static str> {
        ata::read_sectors(lba, count, buf)
    }

    fn write_sectors(&self, lba: u32, count: u8, buf: &[u8]) -> Result<(), &'static str> {
        ata::write_sectors(lba, count, buf)
    }
}

/// The `constanos-data` partition of the ATA disk, when that disk is a whole
/// GPT image (the release image attached to a VM's IDE controller) rather
/// than a bare ext2 filesystem (QEMU's `disk.img`, whose LBA 1 holds no GPT
/// signature). Only the primary GPT is read: the driver issues no IDENTIFY,
/// so it does not know where the backup copy sits.
pub fn ata_data_partition() -> Option<Partition> {
    let table = hal::gpt::read_gpt(&AtaBlockDevice, 0).ok()?;
    let part = hal::gpt::select(&table, usb::DATA_PARTITION_NAME).ok()?;
    crate::serial_println!(
        "ata: GPT disk: using partition {} at LBA {} ({} sectors)",
        part.index, part.first_lba, part.sectors()
    );
    // `select` already refused anything past 32-bit LBAs.
    Partition::new(alloc::boxed::Box::new(AtaBlockDevice), part.first_lba as u32, part.sectors() as u32, false)
}
