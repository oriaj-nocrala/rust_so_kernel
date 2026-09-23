//! GUID Partition Table — pure logic over the `BlockDevice` seam.
//!
//! The boot pendrive carries two partitions: the UEFI image (`boot`) and
//! the ext2 data partition (`constanos-data`). A USB block device hands the
//! filesystem a whole disk, so something has to find where the data
//! partition starts; this module is that something (UEFI 2.10 §5.3).
//!
//! **Both CRCs are checked, and a bad primary falls back to the backup.**
//! The GPT has two copies precisely so a torn write to one does not lose
//! the disk; a reader that only looks at the primary, or that trusts it
//! without its CRC, will one day mount whatever garbage sits at a stale
//! offset. The stick this was written for already had a broken backup once
//! (a `dd` of a 17 MiB image left a backup header pointing at sector
//! 34910), so the failure is not hypothetical — see
//! `docs/storage/usb-msc-plan.md`.
//!
//! The lookup is by partition *name*, like `scripts/sync-usb-data.sh`
//! finds the same partition from the host: a partition index is a fact
//! about how the disk was laid out today. When no partition carries the
//! name, a disk with exactly one Linux-filesystem partition uses that one;
//! two or more is ambiguous and refused, since guessing picks a filesystem
//! to mount read-write.
//!
//! Every length and count in the header comes from the disk, so each is
//! bounded before it sizes a read or an allocation.

use alloc::vec;
use alloc::vec::Vec;

use crate::block::{BlockDevice, SECTOR_SIZE};

pub const SIGNATURE: &[u8; 8] = b"EFI PART";
/// Smallest legal header (UEFI §5.3.2, table 5-5).
const MIN_HEADER_SIZE: usize = 92;
/// Entries array bigger than this is refused rather than allocated. The
/// standard array is 128 × 128 bytes = 16 KiB.
const MAX_ARRAY_BYTES: usize = 1 << 20;
/// Characters in a partition name (72 bytes of UTF-16LE).
const NAME_UNITS: usize = 36;

/// Linux filesystem data, `0FC63DAF-8483-4772-8E79-3D69D8477DE4`, in its
/// on-disk mixed-endian byte order (first three fields little-endian).
pub const TYPE_LINUX_FS: [u8; 16] = [
    0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
];

// ── CRC32 ────────────────────────────────────────────────────────────────────

/// CRC-32/ISO-HDLC (the zlib/Ethernet one: reflected, polynomial
/// 0xEDB88320, init and final XOR 0xFFFFFFFF) — what the GPT specifies.
/// Bitwise rather than table-driven: it runs over ~16 KiB once per mount,
/// and a 1 KiB table is not worth its weight here.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

// ── Header ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub my_lba: u64,
    pub alternate_lba: u64,
    pub first_usable: u64,
    pub last_usable: u64,
    pub entries_lba: u64,
    pub num_entries: u32,
    pub entry_size: u32,
    pub entries_crc: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    BadSignature,
    BadSize(u32),
    BadCrc { stored: u32, computed: u32 },
    /// The header does not say it lives where it was read from — the stale
    /// backup of a disk image `dd`'d onto a bigger device looks exactly
    /// like this.
    WrongLocation { expected: u64, found: u64 },
    BadEntryGeometry { num_entries: u32, entry_size: u32 },
}

/// Parses and validates one header sector (UEFI §5.3.2's checks 1-4).
pub fn parse_header(sector: &[u8], expected_lba: u64) -> Result<Header, HeaderError> {
    if sector.len() < SECTOR_SIZE || &sector[0..8] != SIGNATURE {
        return Err(HeaderError::BadSignature);
    }
    let u32_at = |o: usize| u32::from_le_bytes([sector[o], sector[o + 1], sector[o + 2], sector[o + 3]]);
    let u64_at = |o: usize| u64::from_le_bytes(sector[o..o + 8].try_into().unwrap());

    let header_size = u32_at(12);
    if (header_size as usize) < MIN_HEADER_SIZE || header_size as usize > SECTOR_SIZE {
        return Err(HeaderError::BadSize(header_size));
    }
    let stored = u32_at(16);
    let mut copy = [0u8; SECTOR_SIZE];
    copy[..header_size as usize].copy_from_slice(&sector[..header_size as usize]);
    copy[16..20].fill(0);
    let computed = crc32(&copy[..header_size as usize]);
    if stored != computed {
        return Err(HeaderError::BadCrc { stored, computed });
    }

    let h = Header {
        my_lba: u64_at(24),
        alternate_lba: u64_at(32),
        first_usable: u64_at(40),
        last_usable: u64_at(48),
        entries_lba: u64_at(72),
        num_entries: u32_at(80),
        entry_size: u32_at(84),
        entries_crc: u32_at(88),
    };
    if h.my_lba != expected_lba {
        return Err(HeaderError::WrongLocation { expected: expected_lba, found: h.my_lba });
    }
    // Entry size: 128 × 2^n (§5.3.2). The array must fit the allocation
    // bound — a hostile count times size is never allocated.
    let size_ok = h.entry_size >= 128 && h.entry_size.is_power_of_two() && h.entry_size <= 4096;
    let total = h.num_entries as usize * h.entry_size as usize;
    if !size_ok || h.num_entries == 0 || total > MAX_ARRAY_BYTES {
        return Err(HeaderError::BadEntryGeometry { num_entries: h.num_entries, entry_size: h.entry_size });
    }
    Ok(h)
}

// ── Entries ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partition {
    /// 1-based index in the entry array, as `/dev/sdX<n>` numbers it.
    pub index: u32,
    pub type_guid: [u8; 16],
    pub first_lba: u64,
    /// Inclusive, as the GPT stores it.
    pub last_lba: u64,
    pub name: [u16; NAME_UNITS],
}

impl Partition {
    pub fn sectors(&self) -> u64 {
        self.last_lba - self.first_lba + 1
    }

    /// Compares the UTF-16LE name against an ASCII string. Non-ASCII
    /// names simply never match, which is all a lookup by a known ASCII
    /// label needs.
    pub fn name_is(&self, want: &str) -> bool {
        let len = self.name.iter().position(|&u| u == 0).unwrap_or(NAME_UNITS);
        len == want.len() && self.name[..len].iter().zip(want.bytes()).all(|(&u, b)| u == b as u16)
    }

    /// The name as ASCII into `out`, `?` for anything else; returns the
    /// used prefix. For logs.
    pub fn name_ascii<'a>(&self, out: &'a mut [u8; NAME_UNITS]) -> &'a str {
        let mut n = 0;
        for &u in self.name.iter().take_while(|&&u| u != 0) {
            out[n] = if (0x20..0x7F).contains(&u) { u as u8 } else { b'?' };
            n += 1;
        }
        core::str::from_utf8(&out[..n]).unwrap_or("")
    }
}

/// Decodes the used entries of a validated array. An entry whose range is
/// inverted or outside the usable area is skipped, not trusted.
pub fn parse_entries(array: &[u8], h: &Header) -> Vec<Partition> {
    let mut out = Vec::new();
    let size = h.entry_size as usize;
    for i in 0..h.num_entries as usize {
        let Some(e) = array.get(i * size..i * size + 128) else { break };
        let mut type_guid = [0u8; 16];
        type_guid.copy_from_slice(&e[0..16]);
        if type_guid == [0u8; 16] {
            continue; // unused slot
        }
        let first_lba = u64::from_le_bytes(e[32..40].try_into().unwrap());
        let last_lba = u64::from_le_bytes(e[40..48].try_into().unwrap());
        if first_lba > last_lba || first_lba < h.first_usable || last_lba > h.last_usable {
            continue;
        }
        let mut name = [0u16; NAME_UNITS];
        for (k, unit) in name.iter_mut().enumerate() {
            *unit = u16::from_le_bytes([e[56 + 2 * k], e[57 + 2 * k]]);
        }
        out.push(Partition { index: i as u32 + 1, type_guid, first_lba, last_lba, name });
    }
    out
}

// ── Reading a disk ───────────────────────────────────────────────────────────

/// Which copy of the table was used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Primary,
    Backup,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GptError {
    Io(&'static str),
    /// Neither copy validated. Both reasons kept — "primary CRC bad, backup
    /// in the wrong place" is a very different disk from "no GPT at all".
    NoValidTable { primary: TableError, backup: TableError },
    /// Beyond what a 32-bit-LBA `BlockDevice` can address.
    TooLarge,
    NotFound,
    /// No partition by that name, and more than one Linux-filesystem
    /// partition to fall back to.
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableError {
    Header(HeaderError),
    ArrayCrc { stored: u32, computed: u32 },
    Io,
}

/// A validated table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub copy: Which,
    pub header: Header,
    pub partitions: Vec<Partition>,
}

/// Reads `count` sectors at a 64-bit LBA through the 32-bit, ≤255-sector
/// `BlockDevice` interface.
fn read(dev: &dyn BlockDevice, lba: u64, count: usize, out: &mut [u8]) -> Result<(), &'static str> {
    let mut done = 0usize;
    while done < count {
        let n = (count - done).min(255);
        let at = lba + done as u64;
        let at = u32::try_from(at).map_err(|_| "gpt: LBA beyond 32 bits")?;
        dev.read_sectors(at, n as u8, &mut out[done * SECTOR_SIZE..(done + n) * SECTOR_SIZE])?;
        done += n;
    }
    Ok(())
}

fn read_table(dev: &dyn BlockDevice, header_lba: u64, copy: Which) -> Result<Table, TableError> {
    let mut sector = [0u8; SECTOR_SIZE];
    read(dev, header_lba, 1, &mut sector).map_err(|_| TableError::Io)?;
    let header = parse_header(&sector, header_lba).map_err(TableError::Header)?;

    let bytes = header.num_entries as usize * header.entry_size as usize;
    let sectors = bytes.div_ceil(SECTOR_SIZE);
    let mut array = vec![0u8; sectors * SECTOR_SIZE];
    read(dev, header.entries_lba, sectors, &mut array).map_err(|_| TableError::Io)?;
    let computed = crc32(&array[..bytes]);
    if computed != header.entries_crc {
        return Err(TableError::ArrayCrc { stored: header.entries_crc, computed });
    }
    let partitions = parse_entries(&array[..bytes], &header);
    Ok(Table { copy, header, partitions })
}

/// Reads the partition table: the primary at LBA 1, or — only if the
/// primary fails validation — the backup in the disk's last sector.
pub fn read_gpt(dev: &dyn BlockDevice, total_sectors: u64) -> Result<Table, GptError> {
    let primary = match read_table(dev, 1, Which::Primary) {
        Ok(t) => return Ok(t),
        Err(e) => e,
    };
    let backup = match total_sectors.checked_sub(1) {
        Some(last) if last > 1 => match read_table(dev, last, Which::Backup) {
            Ok(t) => return Ok(t),
            Err(e) => e,
        },
        _ => TableError::Io,
    };
    Err(GptError::NoValidTable { primary, backup })
}

/// Picks the partition to mount: by name, else the only Linux-filesystem
/// partition. Refuses a result a 32-bit-LBA device cannot fully address.
pub fn select<'a>(table: &'a Table, name: &str) -> Result<&'a Partition, GptError> {
    let chosen = match table.partitions.iter().find(|p| p.name_is(name)) {
        Some(p) => p,
        None => {
            let mut linux = table.partitions.iter().filter(|p| p.type_guid == TYPE_LINUX_FS);
            match (linux.next(), linux.next()) {
                (Some(p), None) => p,
                (None, _) => return Err(GptError::NotFound),
                (Some(_), Some(_)) => return Err(GptError::Ambiguous),
            }
        }
    };
    if chosen.last_lba > u32::MAX as u64 {
        return Err(GptError::TooLarge);
    }
    Ok(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::MemDisk;

    /// Real `sfdisk` output (util-linux), laid out like the boot stick:
    /// a 16 MiB disk, `boot` (EFI System) at 34..2081 and
    /// `constanos-data` (Linux filesystem) at 4096..28671. Only the two
    /// table regions are stored: 34 sectors at the start (protective MBR,
    /// primary header, array) and 33 at the end (array, backup header).
    /// Regenerate with the `sfdisk` script in `hal/fixtures/README.md`.
    const HEAD: &[u8] = include_bytes!("../fixtures/gpt-head.bin");
    const TAIL: &[u8] = include_bytes!("../fixtures/gpt-tail.bin");
    const DISK_SECTORS: usize = 32768;

    fn sfdisk_image() -> Vec<u8> {
        let mut img = vec![0u8; DISK_SECTORS * SECTOR_SIZE];
        img[..HEAD.len()].copy_from_slice(HEAD);
        let tail_at = img.len() - TAIL.len();
        img[tail_at..].copy_from_slice(TAIL);
        img
    }

    #[test]
    fn crc32_known_vectors() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926); // the standard check value
        assert_eq!(crc32(b"The quick brown fox jumps over the lazy dog"), 0x414F_A339);
    }

    /// The oracle test: a table written by a real partitioning tool reads
    /// back exactly as `sfdisk -d` reported it.
    #[test]
    fn reads_the_sfdisk_table() {
        let disk = MemDisk::from_vec(sfdisk_image());
        let t = read_gpt(&disk, DISK_SECTORS as u64).unwrap();
        assert_eq!(t.copy, Which::Primary);
        assert_eq!(t.header.first_usable, 34);
        assert_eq!(t.header.last_usable, 32734);
        assert_eq!(t.partitions.len(), 2);

        let boot = &t.partitions[0];
        assert!(boot.name_is("boot"));
        assert_eq!((boot.index, boot.first_lba, boot.sectors()), (1, 34, 2048));

        let data = select(&t, "constanos-data").unwrap();
        assert_eq!((data.index, data.first_lba, data.sectors()), (2, 4096, 24576));
        assert_eq!(data.type_guid, TYPE_LINUX_FS);
        let mut buf = [0u8; NAME_UNITS];
        assert_eq!(data.name_ascii(&mut buf), "constanos-data");
    }

    #[test]
    fn corrupt_primary_header_falls_back_to_backup() {
        let mut img = sfdisk_image();
        // Disk GUID: nothing but the header CRC can notice this byte.
        img[SECTOR_SIZE + 60] ^= 0xFF;
        let disk = MemDisk::from_vec(img);
        let t = read_gpt(&disk, DISK_SECTORS as u64).unwrap();
        assert_eq!(t.copy, Which::Backup);
        assert_eq!(select(&t, "constanos-data").unwrap().first_lba, 4096);
    }

    /// A header whose own CRC is fine but whose *array* was corrupted must
    /// not be believed either — that is what the second CRC is for.
    #[test]
    fn corrupt_primary_array_falls_back_to_backup() {
        let mut img = sfdisk_image();
        img[2 * SECTOR_SIZE + 128 + 32] ^= 0x01; // entry 2's first_lba
        let disk = MemDisk::from_vec(img);
        let t = read_gpt(&disk, DISK_SECTORS as u64).unwrap();
        assert_eq!(t.copy, Which::Backup);
        assert_eq!(select(&t, "constanos-data").unwrap().first_lba, 4096);
    }

    #[test]
    fn both_copies_bad_reports_both_reasons() {
        let mut img = sfdisk_image();
        img[SECTOR_SIZE] = b'X'; // primary signature
        let last = img.len() - SECTOR_SIZE;
        img[last + 30] ^= 0xFF; // backup header CRC
        let disk = MemDisk::from_vec(img);
        match read_gpt(&disk, DISK_SECTORS as u64) {
            Err(GptError::NoValidTable { primary, backup }) => {
                assert_eq!(primary, TableError::Header(HeaderError::BadSignature));
                assert!(matches!(backup, TableError::Header(HeaderError::BadCrc { .. })));
            }
            other => panic!("expected NoValidTable, got {:?}", other),
        }
    }

    /// The stick's own history: an image `dd`'d onto a bigger device, whose
    /// backup header sits mid-disk. Read as "the last sector", it has to be
    /// rejected for claiming a different LBA, not trusted.
    #[test]
    fn backup_in_the_wrong_place_is_rejected() {
        let mut img = sfdisk_image();
        img[SECTOR_SIZE] = b'X';
        img.extend_from_slice(&vec![0u8; 1024 * SECTOR_SIZE]); // disk grew
        let last = img.len() / SECTOR_SIZE - 1;
        // Put the old backup header in the new last sector.
        let old = (DISK_SECTORS - 1) * SECTOR_SIZE;
        let hdr: Vec<u8> = img[old..old + SECTOR_SIZE].to_vec();
        img[last * SECTOR_SIZE..].copy_from_slice(&hdr);
        let disk = MemDisk::from_vec(img);
        match read_gpt(&disk, last as u64 + 1) {
            Err(GptError::NoValidTable { backup, .. }) => assert_eq!(
                backup,
                TableError::Header(HeaderError::WrongLocation {
                    expected: last as u64,
                    found: DISK_SECTORS as u64 - 1
                })
            ),
            other => panic!("expected WrongLocation, got {:?}", other),
        }
    }

    // ── Hand-built tables, for what sfdisk won't produce ────────────────

    fn entry(type_guid: [u8; 16], first: u64, last: u64, name: &str) -> [u8; 128] {
        let mut e = [0u8; 128];
        e[0..16].copy_from_slice(&type_guid);
        e[16] = 1; // any unique GUID
        e[32..40].copy_from_slice(&first.to_le_bytes());
        e[40..48].copy_from_slice(&last.to_le_bytes());
        for (k, b) in name.bytes().enumerate() {
            e[56 + 2 * k] = b;
        }
        e
    }

    /// Minimal valid primary-only disk with the given entries.
    fn build(entries: &[[u8; 128]], num_entries: u32, sectors: usize) -> MemDisk {
        let mut img = vec![0u8; sectors * SECTOR_SIZE];
        let array_bytes = num_entries as usize * 128;
        for (i, e) in entries.iter().enumerate() {
            img[2 * SECTOR_SIZE + i * 128..2 * SECTOR_SIZE + (i + 1) * 128].copy_from_slice(e);
        }
        let array_crc = crc32(&img[2 * SECTOR_SIZE..2 * SECTOR_SIZE + array_bytes]);
        let h = &mut img[SECTOR_SIZE..2 * SECTOR_SIZE];
        h[0..8].copy_from_slice(SIGNATURE);
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&1u64.to_le_bytes());
        h[32..40].copy_from_slice(&(sectors as u64 - 1).to_le_bytes());
        h[40..48].copy_from_slice(&34u64.to_le_bytes());
        h[48..56].copy_from_slice(&(sectors as u64 - 34).to_le_bytes());
        h[72..80].copy_from_slice(&2u64.to_le_bytes());
        h[80..84].copy_from_slice(&num_entries.to_le_bytes());
        h[84..88].copy_from_slice(&128u32.to_le_bytes());
        h[88..92].copy_from_slice(&array_crc.to_le_bytes());
        let crc = crc32(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        MemDisk::from_vec(img)
    }

    #[test]
    fn falls_back_to_the_only_linux_partition() {
        let esp = [0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B];
        let disk = build(&[entry(esp, 34, 99, "boot"), entry(TYPE_LINUX_FS, 100, 199, "renamed")], 4, 400);
        let t = read_gpt(&disk, 400).unwrap();
        assert_eq!(select(&t, "constanos-data").unwrap().first_lba, 100);
    }

    #[test]
    fn two_linux_partitions_without_the_name_is_ambiguous() {
        let disk = build(
            &[entry(TYPE_LINUX_FS, 34, 99, "a"), entry(TYPE_LINUX_FS, 100, 199, "b")],
            4,
            400,
        );
        let t = read_gpt(&disk, 400).unwrap();
        assert_eq!(select(&t, "constanos-data"), Err(GptError::Ambiguous));
        // ...but the name still wins when present.
        assert_eq!(select(&t, "b").unwrap().first_lba, 100);
    }

    #[test]
    fn no_candidate_is_not_found() {
        let disk = build(&[entry([7; 16], 34, 99, "other")], 4, 400);
        let t = read_gpt(&disk, 400).unwrap();
        assert_eq!(select(&t, "constanos-data"), Err(GptError::NotFound));
    }

    /// Entries whose range is inverted or outside the usable area are
    /// dropped — including a name match, which must not win by name alone.
    #[test]
    fn out_of_range_entries_are_skipped() {
        let disk = build(
            &[
                entry(TYPE_LINUX_FS, 200, 100, "constanos-data"), // inverted
                entry(TYPE_LINUX_FS, 10, 99, "constanos-data"),   // before first usable
                entry(TYPE_LINUX_FS, 100, 390, "constanos-data"), // past last usable (366)
            ],
            4,
            400,
        );
        let t = read_gpt(&disk, 400).unwrap();
        assert!(t.partitions.is_empty());
    }

    /// A name that is a prefix, or has the right text past its NUL, is not
    /// the name.
    #[test]
    fn name_comparison_is_exact() {
        let disk = build(&[entry([7; 16], 34, 99, "constanos-data2")], 4, 400);
        let t = read_gpt(&disk, 400).unwrap();
        assert!(!t.partitions[0].name_is("constanos-data"));
        assert!(t.partitions[0].name_is("constanos-data2"));
    }

    #[test]
    fn hostile_entry_geometry_is_refused_before_allocating() {
        let disk = build(&[], 4, 400);
        let mut img = disk.snapshot();
        let h = &mut img[SECTOR_SIZE..2 * SECTOR_SIZE];
        h[80..84].copy_from_slice(&u32::MAX.to_le_bytes()); // 4 G entries
        h[16..20].fill(0);
        let crc = crc32(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            parse_header(&img[SECTOR_SIZE..2 * SECTOR_SIZE], 1),
            Err(HeaderError::BadEntryGeometry { .. })
        ));

        let mut bad_size = [0u8; SECTOR_SIZE];
        bad_size[..8].copy_from_slice(SIGNATURE);
        bad_size[12..16].copy_from_slice(&4096u32.to_le_bytes());
        assert_eq!(parse_header(&bad_size, 1), Err(HeaderError::BadSize(4096)));
    }

    #[test]
    fn partition_beyond_32_bit_lba_is_too_large() {
        let mut p = Partition { index: 1, type_guid: TYPE_LINUX_FS, first_lba: 34, last_lba: 1 << 32, name: [0; NAME_UNITS] };
        let name = "big";
        for (k, b) in name.bytes().enumerate() {
            p.name[k] = b as u16;
        }
        let t = Table {
            copy: Which::Primary,
            header: parse_header(&build(&[], 4, 400).snapshot()[SECTOR_SIZE..2 * SECTOR_SIZE], 1).unwrap(),
            partitions: vec![p],
        };
        assert_eq!(select(&t, "big"), Err(GptError::TooLarge));
    }

    /// The real boot pendrive (SanDisk 3.2Gen1, 60088320 sectors): its two
    /// table regions as read off the device with `dd` on 2026-09-23. A
    /// 30 GB `MemDisk` is out of the question, so a sparse device serves
    /// those regions and zeros everywhere else.
    struct SparseStick;
    const STICK_HEAD: &[u8] = include_bytes!("../fixtures/stick-head.bin");
    const STICK_TAIL: &[u8] = include_bytes!("../fixtures/stick-tail.bin");
    const STICK_SECTORS: u64 = 60_088_320;

    impl BlockDevice for SparseStick {
        fn present(&self) -> bool {
            true
        }
        fn read_sectors(&self, lba: u32, count: u8, buf: &mut [u8]) -> Result<(), &'static str> {
            let n = if count == 0 { 256 } else { count as usize };
            let tail_start = STICK_SECTORS as usize - STICK_TAIL.len() / SECTOR_SIZE;
            for i in 0..n {
                let s = lba as usize + i;
                let out = &mut buf[i * SECTOR_SIZE..(i + 1) * SECTOR_SIZE];
                if s * SECTOR_SIZE < STICK_HEAD.len() {
                    out.copy_from_slice(&STICK_HEAD[s * SECTOR_SIZE..(s + 1) * SECTOR_SIZE]);
                } else if s >= tail_start && (s as u64) < STICK_SECTORS {
                    let k = s - tail_start;
                    out.copy_from_slice(&STICK_TAIL[k * SECTOR_SIZE..(k + 1) * SECTOR_SIZE]);
                } else if (s as u64) < STICK_SECTORS {
                    out.fill(0);
                } else {
                    return Err("past end");
                }
            }
            Ok(())
        }
        fn write_sectors(&self, _: u32, _: u8, _: &[u8]) -> Result<(), &'static str> {
            Err("read-only fixture")
        }
    }

    #[test]
    fn reads_the_real_boot_stick() {
        let t = read_gpt(&SparseStick, STICK_SECTORS).unwrap();
        assert_eq!(t.copy, Which::Primary);
        let p = select(&t, "constanos-data").unwrap();
        assert_eq!((p.index, p.first_lba, p.sectors()), (2, 36864, 4_194_304));
        assert_eq!(p.type_guid, TYPE_LINUX_FS);
        assert!(t.partitions[0].name_is("boot"));
        assert_eq!((t.partitions[0].first_lba, t.partitions[0].sectors()), (34, 34816));
    }

    /// And its backup — the copy that was broken once (see the module doc)
    /// and was relocated with `sfdisk --relocate`. It must now validate on
    /// its own, at the device's real last sector.
    #[test]
    fn the_real_stick_backup_table_is_valid_on_its_own() {
        let t = read_table(&SparseStick, STICK_SECTORS - 1, Which::Backup).unwrap();
        assert_eq!(select(&t, "constanos-data").unwrap().first_lba, 36864);
        assert_eq!(t.header.alternate_lba, 1);
    }
}
