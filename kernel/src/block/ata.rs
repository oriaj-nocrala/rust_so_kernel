// kernel/src/block/ata.rs
//
// ATA PIO driver, polling mode (no IRQ), LBA28, secondary channel, master
// drive only. Just enough to back a read-only ext2 mount (fs::ext2).
//
// Deliberately targets the SECONDARY IDE channel (0x170/0x376), not the
// primary (0x1F0/0x3F6) the UEFI boot disk sits on — see src/main.rs, which
// attaches the ext2 disk image explicitly to `ide.1` via `-device
// ide-hd,bus=ide.1`. Using a separate channel entirely sidesteps any
// ambiguity about master/slave ordering on the boot disk's own channel.
//
// No PCI/virtio needed: QEMU's default `pc` (i440fx) machine always
// exposes the legacy PIIX3 IDE controller at these fixed ISA ports.

use spin::Mutex;
use x86_64::instructions::port::Port;

const DATA: u16          = 0x170;
const ERROR_FEATURES: u16 = 0x171;
const SECTOR_COUNT: u16   = 0x172;
const LBA_LOW: u16        = 0x173;
const LBA_MID: u16        = 0x174;
const LBA_HIGH: u16       = 0x175;
const DRIVE_HEAD: u16     = 0x176;
const COMMAND_STATUS: u16 = 0x177;
const ALT_STATUS: u16     = 0x376;

const STATUS_ERR: u8 = 1 << 0;
const STATUS_DRQ: u8 = 1 << 3;
const STATUS_BSY: u8 = 1 << 7;

const CMD_READ_SECTORS: u8 = 0x20;
const CMD_WRITE_SECTORS: u8 = 0x30;
const CMD_CACHE_FLUSH: u8 = 0xE7;

pub const SECTOR_SIZE: usize = 512;

/// Guards the whole channel — PIO transfers are inherently sequential
/// (one command in flight at a time), and there's no IRQ-driven queuing
/// here to make concurrent access meaningful anyway.
static ATA_LOCK: Mutex<()> = Mutex::new(());

/// ~400ns delay via 4 wasted status-register reads — the standard ATA
/// trick for letting a drive select settle before trusting its status.
fn wait_400ns() {
    let mut alt: Port<u8> = Port::new(ALT_STATUS);
    for _ in 0..4 {
        unsafe { alt.read(); }
    }
}

fn wait_not_busy() -> Result<(), &'static str> {
    let mut status: Port<u8> = Port::new(COMMAND_STATUS);
    for _ in 0..1_000_000u32 {
        let s = unsafe { status.read() };
        if s & STATUS_BSY == 0 {
            if s & STATUS_ERR != 0 {
                return Err("ata: ERR set while waiting for BSY clear");
            }
            return Ok(());
        }
    }
    Err("ata: timeout waiting for BSY clear")
}

fn wait_drq() -> Result<(), &'static str> {
    let mut status: Port<u8> = Port::new(COMMAND_STATUS);
    for _ in 0..1_000_000u32 {
        let s = unsafe { status.read() };
        if s & STATUS_ERR != 0 {
            return Err("ata: ERR set while waiting for DRQ");
        }
        if s & STATUS_DRQ != 0 {
            return Ok(());
        }
    }
    Err("ata: timeout waiting for DRQ")
}

/// Read `count` sectors (512 bytes each) starting at `lba` into `buf`.
/// `buf.len()` must be at least `count as usize * SECTOR_SIZE`.
///
/// `count == 0` means 256 sectors, per the LBA28 PIO command convention —
/// not used by `fs::ext2` today (block reads are always 1-8 sectors), but
/// documented here since it's a sharp edge in the ATA spec itself, not
/// something this driver adds.
pub fn read_sectors(lba: u32, count: u8, buf: &mut [u8]) -> Result<(), &'static str> {
    let n = if count == 0 { 256 } else { count as usize };
    assert!(buf.len() >= n * SECTOR_SIZE, "ata::read_sectors: buf too small");
    assert!(lba & 0xF000_0000 == 0, "ata::read_sectors: LBA28 overflow");

    let _guard = ATA_LOCK.lock();

    unsafe {
        let mut drive_head: Port<u8> = Port::new(DRIVE_HEAD);
        let mut sector_count: Port<u8> = Port::new(SECTOR_COUNT);
        let mut lba_low: Port<u8> = Port::new(LBA_LOW);
        let mut lba_mid: Port<u8> = Port::new(LBA_MID);
        let mut lba_high: Port<u8> = Port::new(LBA_HIGH);
        let mut command: Port<u8> = Port::new(COMMAND_STATUS);
        let mut data: Port<u16> = Port::new(DATA);
        let mut error: Port<u8> = Port::new(ERROR_FEATURES);

        // Select master drive on this channel, LBA mode, top 4 LBA bits.
        drive_head.write(0xE0 | ((lba >> 24) & 0x0F) as u8);
        wait_400ns();
        wait_not_busy()?;

        sector_count.write(count);
        lba_low.write((lba & 0xFF) as u8);
        lba_mid.write(((lba >> 8) & 0xFF) as u8);
        lba_high.write(((lba >> 16) & 0xFF) as u8);
        command.write(CMD_READ_SECTORS);

        for sector in 0..n {
            if let Err(e) = wait_drq() {
                let _ = error.read(); // clear/inspect error register (debug aid only)
                return Err(e);
            }
            let base = sector * SECTOR_SIZE;
            for i in 0..256 {
                let word = data.read();
                buf[base + i * 2] = (word & 0xFF) as u8;
                buf[base + i * 2 + 1] = (word >> 8) as u8;
            }
        }
    }

    Ok(())
}

/// Write `count` sectors (512 bytes each) from `buf` starting at `lba`.
/// `buf.len()` must be at least `count as usize * SECTOR_SIZE`. Same
/// LBA28/`count == 0` convention as `read_sectors`. Issues a CACHE FLUSH
/// (0xE7) after the transfer so a write is actually on stable media before
/// this returns — matters once `fs::ext2` starts persisting bitmaps and
/// inodes here, unlike the read-only path this driver started as.
pub fn write_sectors(lba: u32, count: u8, buf: &[u8]) -> Result<(), &'static str> {
    let n = if count == 0 { 256 } else { count as usize };
    assert!(buf.len() >= n * SECTOR_SIZE, "ata::write_sectors: buf too small");
    assert!(lba & 0xF000_0000 == 0, "ata::write_sectors: LBA28 overflow");

    let _guard = ATA_LOCK.lock();

    unsafe {
        let mut drive_head: Port<u8> = Port::new(DRIVE_HEAD);
        let mut sector_count: Port<u8> = Port::new(SECTOR_COUNT);
        let mut lba_low: Port<u8> = Port::new(LBA_LOW);
        let mut lba_mid: Port<u8> = Port::new(LBA_MID);
        let mut lba_high: Port<u8> = Port::new(LBA_HIGH);
        let mut command: Port<u8> = Port::new(COMMAND_STATUS);
        let mut data: Port<u16> = Port::new(DATA);
        let mut error: Port<u8> = Port::new(ERROR_FEATURES);

        drive_head.write(0xE0 | ((lba >> 24) & 0x0F) as u8);
        wait_400ns();
        wait_not_busy()?;

        sector_count.write(count);
        lba_low.write((lba & 0xFF) as u8);
        lba_mid.write(((lba >> 8) & 0xFF) as u8);
        lba_high.write(((lba >> 16) & 0xFF) as u8);
        command.write(CMD_WRITE_SECTORS);

        for sector in 0..n {
            if let Err(e) = wait_drq() {
                let _ = error.read();
                return Err(e);
            }
            let base = sector * SECTOR_SIZE;
            for i in 0..256 {
                let word = (buf[base + i * 2] as u16) | ((buf[base + i * 2 + 1] as u16) << 8);
                data.write(word);
            }
            // Drive clears DRQ and processes the sector before it will
            // raise DRQ again for the next one (or finish the command on
            // the last sector) — wait for BSY to drop each time, same as
            // the read path's per-sector DRQ wait, just mirrored.
            wait_not_busy()?;
        }

        command.write(CMD_CACHE_FLUSH);
        wait_not_busy()?;
    }

    Ok(())
}

/// True if a drive answers on the secondary channel at all (status register
/// isn't stuck at 0xFF, the standard "nothing here" floating-bus read).
/// Used by `fs::ext2::init` to fail fast with a clear message instead of
/// spinning through `wait_not_busy`'s full timeout when no disk is attached.
pub fn present() -> bool {
    let _guard = ATA_LOCK.lock();
    let mut status: Port<u8> = Port::new(COMMAND_STATUS);
    unsafe { status.read() != 0xFF }
}
