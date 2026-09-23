// kernel/src/block/usb.rs
//
// The USB pendrive as a `hal::block::BlockDevice` — the whole disk, as
// `usb::storage_read`/`storage_write` see it. `data_partition()` then finds
// the ext2 partition on it through the GPT (`hal::gpt`, host-tested) and
// wraps it in a `hal::block::Partition`, which is what `fs::ext2` mounts.
//
// The partition is found by *name* (`constanos-data`), the same way
// `scripts/sync-usb-data.sh` finds it from the host — see `hal::gpt`'s
// module doc for the fallback when the name is missing.

use alloc::boxed::Box;

use hal::block::{BlockDevice, Partition, SECTOR_SIZE};

/// GPT partition name of the data partition — must match
/// `scripts/sync-usb-data.sh` and `docs/storage/usb-msc-plan.md`.
pub const DATA_PARTITION_NAME: &str = "constanos-data";

pub struct UsbBlockDevice {
    dev: crate::usb::Storage,
    /// Writes give up (`Err`) instead of waiting when the controller is
    /// busy — see [`crate::usb::try_storage_write`]. Only the log
    /// partition's panic-time handle sets it.
    nonblocking: bool,
}

impl UsbBlockDevice {
    pub fn new(dev: crate::usb::Storage) -> Self {
        UsbBlockDevice { dev, nonblocking: false }
    }

    pub fn new_nonblocking(dev: crate::usb::Storage) -> Self {
        UsbBlockDevice { dev, nonblocking: true }
    }
}

impl BlockDevice for UsbBlockDevice {
    fn present(&self) -> bool {
        true
    }

    fn read_sectors(&self, lba: u32, count: u8, buf: &mut [u8]) -> Result<(), &'static str> {
        let n = if count == 0 { 256 } else { count as usize };
        if buf.len() < n * SECTOR_SIZE {
            return Err("usb-storage: buffer too small");
        }
        crate::usb::storage_read(&self.dev, lba, n, buf).map_err(|e| {
            crate::serial_println!("usb-storage: read lba={} count={} failed: {:?}", lba, n, e);
            "usb-storage: read failed"
        })
    }

    fn write_sectors(&self, lba: u32, count: u8, buf: &[u8]) -> Result<(), &'static str> {
        let n = if count == 0 { 256 } else { count as usize };
        if buf.len() < n * SECTOR_SIZE {
            return Err("usb-storage: buffer too small");
        }
        if self.nonblocking {
            // No logging on this path: its one caller is the panic
            // handler, which must not take the `SERIAL` lock.
            return match crate::usb::try_storage_write(&self.dev, lba, n, buf) {
                Some(Ok(())) => Ok(()),
                Some(Err(_)) => Err("usb-storage: write failed"),
                None => Err("usb-storage: controller busy"),
            };
        }
        crate::usb::storage_write(&self.dev, lba, n, buf).map_err(|e| {
            crate::serial_println!("usb-storage: write lba={} count={} failed: {:?}", lba, n, e);
            "usb-storage: write failed"
        })
    }
}

/// The data partition of the boot pendrive, read-only, ready to mount — or
/// why not. Logs which GPT copy was used and where the partition sits, so a
/// boot on the target machine says on screen what it found.
pub fn data_partition() -> Result<Partition, &'static str> {
    let dev = crate::usb::storage().ok_or("no USB mass-storage device")?;
    let disk = UsbBlockDevice::new(dev);

    let table = hal::gpt::read_gpt(&disk, dev.sectors).map_err(|e| {
        crate::serial_println!("usb-storage: no usable GPT: {:?}", e);
        "no usable GPT on the USB stick"
    })?;
    let part = hal::gpt::select(&table, DATA_PARTITION_NAME).map_err(|e| {
        crate::serial_println!("usb-storage: no '{}' partition: {:?}", DATA_PARTITION_NAME, e);
        "no data partition on the USB stick"
    })?;

    let mut name = [0u8; 36];
    crate::serial_println!(
        "usb-storage: GPT ({:?} copy): using partition {} '{}' at LBA {} ({} sectors)",
        table.copy, part.index, part.name_ascii(&mut name), part.first_lba, part.sectors()
    );
    // `select` already refused anything past 32-bit LBAs.
    Partition::new(Box::new(disk), part.first_lba as u32, part.sectors() as u32, true)
        .ok_or("data partition does not fit 32-bit LBAs")
}

/// The partition named exactly `name` on the boot pendrive, as
/// `(first_lba, sectors)` — no fallback to "the only Linux partition" the
/// way [`data_partition`] has, because the caller writes raw sectors.
pub fn partition_by_name(name: &str) -> Result<(crate::usb::Storage, u32, u32), &'static str> {
    let dev = crate::usb::storage().ok_or("no USB mass-storage device")?;
    let disk = UsbBlockDevice::new(dev);
    let table = hal::gpt::read_gpt(&disk, dev.sectors).map_err(|_| "no usable GPT on the USB stick")?;
    let part = table
        .partitions
        .iter()
        .find(|p| p.name_is(name))
        .ok_or("no partition by that name")?;
    let first = u32::try_from(part.first_lba).map_err(|_| "partition beyond 32-bit LBAs")?;
    let sectors = u32::try_from(part.sectors()).map_err(|_| "partition beyond 32-bit LBAs")?;
    first.checked_add(sectors).ok_or("partition beyond 32-bit LBAs")?;
    Ok((dev, first, sectors))
}
