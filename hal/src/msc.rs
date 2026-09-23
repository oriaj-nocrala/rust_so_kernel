//! USB Mass Storage, Bulk-Only Transport + SCSI — pure logic, no seam.
//!
//! A USB pendrive speaks SCSI commands wrapped in a three-phase envelope
//! (USB Mass Storage Class Bulk-Only Transport 1.0, "BOT"):
//!
//! 1. the host sends a 31-byte **Command Block Wrapper** (CBW) on the bulk
//!    OUT endpoint, carrying a SCSI Command Descriptor Block (CDB);
//! 2. an optional data phase moves the payload on bulk IN or bulk OUT;
//! 3. the device answers with a 13-byte **Command Status Wrapper** (CSW) on
//!    bulk IN.
//!
//! Everything here is byte layout: building CBWs and CDBs, validating CSWs,
//! and decoding the three SCSI responses the driver reads (INQUIRY, READ
//! CAPACITY(10), REQUEST SENSE). None of it needs hardware, so it lives here
//! where `cargo test` reaches it; `kernel/src/usb/msc.rs` only moves the
//! bytes. Same "decide, don't do" split as the rest of `hal`.
//!
//! **CBW fields are little-endian, CDB fields are big-endian.** That is not
//! a typo in either spec: the wrapper is USB (LE) and the block inside it
//! is SCSI (BE). It is the single easiest thing in this protocol to get
//! wrong, which is why both have their own tests.
//!
//! Responses come from whatever device was plugged in, so every parser
//! here checks lengths before reading — the same anti-OOB discipline as
//! `hal::usb`'s descriptor walk.

// ── Interface identification (USB MSC Overview 1.4, §2 and §3) ───────────────

/// `bInterfaceClass` for mass storage.
pub const CLASS_MASS_STORAGE: u8 = 0x08;
/// `bInterfaceSubClass`: "SCSI transparent command set" — what every
/// pendrive reports.
pub const SUBCLASS_SCSI: u8 = 0x06;
/// `bInterfaceProtocol`: Bulk-Only Transport. (UAS, protocol 0x62, is the
/// streams-based successor; a UAS-capable stick still offers a BOT
/// alternate setting 0, which is the one this driver takes.)
pub const PROTOCOL_BULK_ONLY: u8 = 0x50;

/// Class-specific request `Bulk-Only Mass Storage Reset` (BOT §3.1).
pub const REQUEST_BOMS_RESET: u8 = 0xFF;
/// Class-specific request `Get Max LUN` (BOT §3.2).
pub const REQUEST_GET_MAX_LUN: u8 = 0xFE;

// ── Command Block Wrapper (BOT §5.1) ─────────────────────────────────────────

pub const CBW_LEN: usize = 31;
pub const CBW_SIGNATURE: u32 = 0x4342_5355; // "USBC", little-endian
/// `bmCBWFlags` bit 7: data phase is device-to-host.
const CBW_FLAG_IN: u8 = 0x80;
/// A CDB is 1..=16 bytes (BOT §5.1, `bCBWCBLength`).
pub const MAX_CDB_LEN: usize = 16;

/// Builds a CBW. `transfer_len` is the number of bytes the host expects in
/// the data phase (0 for none); `data_in` says which direction that phase
/// runs. Returns `None` for a CDB that does not fit the wrapper — never
/// silently truncated, since a truncated READ(10) is a read of the wrong
/// sectors.
pub fn build_cbw(tag: u32, transfer_len: u32, data_in: bool, lun: u8, cdb: &[u8]) -> Option<[u8; CBW_LEN]> {
    if cdb.is_empty() || cdb.len() > MAX_CDB_LEN || lun > 0x0F {
        return None;
    }
    let mut w = [0u8; CBW_LEN];
    w[0..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes());
    w[4..8].copy_from_slice(&tag.to_le_bytes());
    w[8..12].copy_from_slice(&transfer_len.to_le_bytes());
    w[12] = if data_in && transfer_len > 0 { CBW_FLAG_IN } else { 0 };
    w[13] = lun;
    w[14] = cdb.len() as u8;
    w[15..15 + cdb.len()].copy_from_slice(cdb);
    Some(w)
}

// ── Command Status Wrapper (BOT §5.2, §6.3) ──────────────────────────────────

pub const CSW_LEN: usize = 13;
pub const CSW_SIGNATURE: u32 = 0x5342_5355; // "USBS", little-endian

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CswStatus {
    /// The command succeeded.
    Passed,
    /// The command failed; REQUEST SENSE says why.
    Failed,
    /// The device and host disagree about the phase they are in. The only
    /// recovery is a Reset Recovery (BOT §5.3.4): Bulk-Only Mass Storage
    /// Reset plus clearing both endpoints' halts.
    PhaseError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Csw {
    pub tag: u32,
    /// `dCSWDataResidue`: bytes of the data phase *not* transferred.
    pub residue: u32,
    pub status: CswStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CswError {
    /// Not 13 bytes — BOT §6.3.1 makes this "not valid" outright.
    BadLength(usize),
    BadSignature(u32),
    /// A CSW for some other command: the host and device are out of step.
    TagMismatch { expected: u32, got: u32 },
    /// A status byte outside 0..=2 (BOT §6.3.2: "not meaningful").
    BadStatus(u8),
    /// Residue larger than what was asked for (BOT §6.3.2: "not
    /// meaningful" unless the status is Phase Error).
    BadResidue { residue: u32, expected: u32 },
}

/// Validates a CSW against the CBW it answers (BOT §6.3). Both halves of
/// the spec's test are applied — *valid* (length, signature, tag) and
/// *meaningful* (status in range, residue not larger than the request) —
/// because a CSW that fails either leaves the transport in an unknown state
/// and must end in a Reset Recovery, not be believed.
pub fn parse_csw(buf: &[u8], expected_tag: u32, expected_len: u32) -> Result<Csw, CswError> {
    if buf.len() != CSW_LEN {
        return Err(CswError::BadLength(buf.len()));
    }
    let sig = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if sig != CSW_SIGNATURE {
        return Err(CswError::BadSignature(sig));
    }
    let tag = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if tag != expected_tag {
        return Err(CswError::TagMismatch { expected: expected_tag, got: tag });
    }
    let residue = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let status = match buf[12] {
        0 => CswStatus::Passed,
        1 => CswStatus::Failed,
        2 => CswStatus::PhaseError,
        other => return Err(CswError::BadStatus(other)),
    };
    if status != CswStatus::PhaseError && residue > expected_len {
        return Err(CswError::BadResidue { residue, expected: expected_len });
    }
    Ok(Csw { tag, residue, status })
}

// ── SCSI command descriptor blocks (SPC-4 / SBC-3) ───────────────────────────

pub const OP_TEST_UNIT_READY: u8 = 0x00;
pub const OP_REQUEST_SENSE: u8 = 0x03;
pub const OP_INQUIRY: u8 = 0x12;
pub const OP_READ_CAPACITY_10: u8 = 0x25;
pub const OP_READ_10: u8 = 0x28;
pub const OP_WRITE_10: u8 = 0x2A;

/// Standard INQUIRY data is 36 bytes; asking for exactly that is what
/// Linux does, and some devices misbehave when asked for more.
pub const INQUIRY_LEN: u8 = 36;
/// Fixed-format sense data is 18 bytes.
pub const SENSE_LEN: u8 = 18;
pub const READ_CAPACITY_10_LEN: u32 = 8;

pub fn test_unit_ready() -> [u8; 6] {
    [OP_TEST_UNIT_READY, 0, 0, 0, 0, 0]
}

pub fn request_sense() -> [u8; 6] {
    [OP_REQUEST_SENSE, 0, 0, 0, SENSE_LEN, 0]
}

pub fn inquiry() -> [u8; 6] {
    [OP_INQUIRY, 0, 0, 0, INQUIRY_LEN, 0]
}

pub fn read_capacity_10() -> [u8; 10] {
    [OP_READ_CAPACITY_10, 0, 0, 0, 0, 0, 0, 0, 0, 0]
}

/// READ(10): `blocks` logical blocks starting at `lba`. Big-endian fields.
pub fn read_10(lba: u32, blocks: u16) -> [u8; 10] {
    rw_10(OP_READ_10, lba, blocks)
}

/// WRITE(10): same layout as READ(10).
pub fn write_10(lba: u32, blocks: u16) -> [u8; 10] {
    rw_10(OP_WRITE_10, lba, blocks)
}

fn rw_10(op: u8, lba: u32, blocks: u16) -> [u8; 10] {
    let l = lba.to_be_bytes();
    let b = blocks.to_be_bytes();
    [op, 0, l[0], l[1], l[2], l[3], 0, b[0], b[1], 0]
}

// ── SCSI responses ───────────────────────────────────────────────────────────

/// The standard INQUIRY fields worth logging. The strings are fixed-width,
/// space-padded ASCII; `trim` gives the printable part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inquiry {
    /// Peripheral device type (bits 4:0 of byte 0). 0x00 is a
    /// direct-access block device — the only kind this driver reads.
    pub device_type: u8,
    pub removable: bool,
    pub vendor: [u8; 8],
    pub product: [u8; 16],
    pub revision: [u8; 4],
}

pub const DEVICE_TYPE_DIRECT_ACCESS: u8 = 0x00;

pub fn parse_inquiry(buf: &[u8]) -> Option<Inquiry> {
    if buf.len() < INQUIRY_LEN as usize {
        return None;
    }
    let mut vendor = [0u8; 8];
    let mut product = [0u8; 16];
    let mut revision = [0u8; 4];
    vendor.copy_from_slice(&buf[8..16]);
    product.copy_from_slice(&buf[16..32]);
    revision.copy_from_slice(&buf[32..36]);
    Some(Inquiry {
        device_type: buf[0] & 0x1F,
        removable: buf[1] & 0x80 != 0,
        vendor,
        product,
        revision,
    })
}

/// A fixed-width INQUIRY string as printable text: trailing spaces and
/// NULs removed, and anything non-ASCII-printable (a hostile or broken
/// device) rejected in favour of `"?"` rather than written to a log.
pub fn trim(field: &[u8]) -> &str {
    let end = field.iter().rposition(|&b| b != b' ' && b != 0).map_or(0, |i| i + 1);
    let s = &field[..end];
    if s.iter().all(|&b| (0x20..0x7F).contains(&b)) {
        // All printable ASCII, so this cannot fail.
        core::str::from_utf8(s).unwrap_or("?")
    } else {
        "?"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    /// Number of logical blocks — READ CAPACITY returns the *last* LBA,
    /// so this is that plus one.
    pub blocks: u64,
    pub block_size: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityError {
    Short,
    /// Last LBA `0xFFFF_FFFF`: the device is over 2 TiB and only READ
    /// CAPACITY(16) can say how big. Out of scope — this kernel's
    /// `BlockDevice` is LBA32 anyway.
    NeedsCapacity16,
    ZeroBlockSize,
}

pub fn parse_read_capacity_10(buf: &[u8]) -> Result<Capacity, CapacityError> {
    if buf.len() < READ_CAPACITY_10_LEN as usize {
        return Err(CapacityError::Short);
    }
    let last = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let block_size = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if last == u32::MAX {
        return Err(CapacityError::NeedsCapacity16);
    }
    if block_size == 0 {
        return Err(CapacityError::ZeroBlockSize);
    }
    Ok(Capacity { blocks: last as u64 + 1, block_size })
}

/// Fixed-format sense data (SPC-4 §4.5.3): the sense key plus the
/// additional sense code and qualifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sense {
    pub key: u8,
    pub asc: u8,
    pub ascq: u8,
}

pub const SENSE_NO_SENSE: u8 = 0x0;
pub const SENSE_NOT_READY: u8 = 0x2;
pub const SENSE_MEDIUM_ERROR: u8 = 0x3;
pub const SENSE_ILLEGAL_REQUEST: u8 = 0x5;
pub const SENSE_UNIT_ATTENTION: u8 = 0x6;

pub fn parse_sense(buf: &[u8]) -> Option<Sense> {
    // Response code 0x70 (current) or 0x71 (deferred), valid bit ignored.
    // Descriptor-format sense (0x72/0x73) lays out differently; a BOT
    // pendrive answering REQUEST SENSE with DESC=0 never sends it.
    if buf.len() < 14 {
        return None;
    }
    match buf[0] & 0x7F {
        0x70 | 0x71 => {}
        _ => return None,
    }
    Some(Sense { key: buf[2] & 0x0F, asc: buf[12], ascq: buf[13] })
}

/// Whether a failed command is worth simply retrying. UNIT ATTENTION is
/// how a device announces "I was reset / the medium changed" — the first
/// command after power-on routinely fails with it and the second succeeds.
/// NOT READY with ASC 0x04 ("becoming ready") is the same story, slower.
pub fn sense_is_transient(s: Sense) -> bool {
    s.key == SENSE_UNIT_ATTENTION || (s.key == SENSE_NOT_READY && s.asc == 0x04)
}

/// A short name for a sense key, for logs read off a screen.
pub fn describe_sense_key(key: u8) -> &'static str {
    match key {
        0x0 => "NoSense",
        0x1 => "RecoveredError",
        0x2 => "NotReady",
        0x3 => "MediumError",
        0x4 => "HardwareError",
        0x5 => "IllegalRequest",
        0x6 => "UnitAttention",
        0x7 => "DataProtect",
        0xB => "AbortedCommand",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── CBW ─────────────────────────────────────────────────────────────

    /// A READ(10) CBW byte for byte, as Linux's usb-storage sends it and as
    /// it appears in a USB capture: every field position checked, both
    /// endiannesses in one wrapper.
    #[test]
    fn cbw_for_read10_matches_the_wire_format() {
        let cdb = read_10(0x0000_9000, 128);
        let w = build_cbw(0x1234_5678, 128 * 512, true, 0, &cdb).unwrap();
        assert_eq!(&w[0..4], b"USBC");
        assert_eq!(&w[4..8], &[0x78, 0x56, 0x34, 0x12]); // tag, LE
        assert_eq!(&w[8..12], &[0x00, 0x00, 0x01, 0x00]); // 65536, LE
        assert_eq!(w[12], 0x80); // data IN
        assert_eq!(w[13], 0); // LUN
        assert_eq!(w[14], 10); // CDB length
        assert_eq!(&w[15..25], &[0x28, 0, 0x00, 0x00, 0x90, 0x00, 0, 0x00, 0x80, 0]);
        assert!(w[25..].iter().all(|&b| b == 0), "unused CDB bytes must be zero");
    }

    #[test]
    fn cbw_direction_flag_follows_data_phase() {
        let out = build_cbw(1, 512, false, 0, &write_10(0, 1)).unwrap();
        assert_eq!(out[12], 0);
        // No data phase: the direction bit means nothing and BOT §5.1 says
        // the device shall ignore it, but zero is what hosts send.
        let none = build_cbw(1, 0, true, 0, &test_unit_ready()).unwrap();
        assert_eq!(none[12], 0);
    }

    #[test]
    fn cbw_rejects_oversized_or_empty_cdb_and_bad_lun() {
        assert!(build_cbw(1, 0, false, 0, &[]).is_none());
        assert!(build_cbw(1, 0, false, 0, &[0u8; 17]).is_none());
        assert!(build_cbw(1, 0, false, 16, &test_unit_ready()).is_none());
        assert!(build_cbw(1, 0, false, 0, &[0u8; 16]).is_some());
    }

    // ── CSW ─────────────────────────────────────────────────────────────

    fn csw(tag: u32, residue: u32, status: u8) -> [u8; CSW_LEN] {
        let mut b = [0u8; CSW_LEN];
        b[0..4].copy_from_slice(b"USBS");
        b[4..8].copy_from_slice(&tag.to_le_bytes());
        b[8..12].copy_from_slice(&residue.to_le_bytes());
        b[12] = status;
        b
    }

    #[test]
    fn csw_passed_and_failed_parse() {
        assert_eq!(
            parse_csw(&csw(7, 0, 0), 7, 512),
            Ok(Csw { tag: 7, residue: 0, status: CswStatus::Passed })
        );
        assert_eq!(parse_csw(&csw(7, 512, 1), 7, 512).unwrap().status, CswStatus::Failed);
        // Phase error is meaningful regardless of residue.
        assert_eq!(parse_csw(&csw(7, 9999, 2), 7, 512).unwrap().status, CswStatus::PhaseError);
    }

    #[test]
    fn csw_invalid_forms_are_rejected() {
        assert_eq!(parse_csw(&csw(7, 0, 0)[..12], 7, 0), Err(CswError::BadLength(12)));
        let mut long = [0u8; 14];
        long[..13].copy_from_slice(&csw(7, 0, 0));
        assert_eq!(parse_csw(&long, 7, 0), Err(CswError::BadLength(14)));

        let mut bad_sig = csw(7, 0, 0);
        bad_sig[0] = b'X';
        assert!(matches!(parse_csw(&bad_sig, 7, 0), Err(CswError::BadSignature(_))));

        assert_eq!(
            parse_csw(&csw(8, 0, 0), 7, 0),
            Err(CswError::TagMismatch { expected: 7, got: 8 })
        );
        assert_eq!(parse_csw(&csw(7, 0, 3), 7, 0), Err(CswError::BadStatus(3)));
        assert_eq!(
            parse_csw(&csw(7, 513, 0), 7, 512),
            Err(CswError::BadResidue { residue: 513, expected: 512 })
        );
    }

    /// A CBW's own signature must not pass as a CSW — the wrappers differ
    /// by one byte, and a device echoing the CBW back is a real failure.
    #[test]
    fn cbw_is_not_a_csw() {
        let w = build_cbw(7, 0, false, 0, &test_unit_ready()).unwrap();
        assert!(matches!(parse_csw(&w[..13], 7, 0), Err(CswError::BadSignature(_))));
    }

    // ── CDBs ────────────────────────────────────────────────────────────

    #[test]
    fn rw10_is_big_endian() {
        assert_eq!(read_10(0x0102_0304, 0x0506), [0x28, 0, 1, 2, 3, 4, 0, 5, 6, 0]);
        assert_eq!(write_10(0x0102_0304, 0x0506), [0x2A, 0, 1, 2, 3, 4, 0, 5, 6, 0]);
    }

    #[test]
    fn fixed_cdbs() {
        assert_eq!(test_unit_ready(), [0; 6]);
        assert_eq!(inquiry(), [0x12, 0, 0, 0, 36, 0]);
        assert_eq!(request_sense(), [0x03, 0, 0, 0, 18, 0]);
        assert_eq!(read_capacity_10()[0], 0x25);
    }

    // ── Responses ───────────────────────────────────────────────────────

    #[test]
    fn inquiry_of_a_sandisk_stick() {
        let mut buf = [0u8; 36];
        buf[0] = 0x00;
        buf[1] = 0x80;
        buf[8..16].copy_from_slice(b"SanDisk ");
        buf[16..32].copy_from_slice(b"Cruzer Blade    ");
        buf[32..36].copy_from_slice(b"1.00");
        let q = parse_inquiry(&buf).unwrap();
        assert_eq!(q.device_type, DEVICE_TYPE_DIRECT_ACCESS);
        assert!(q.removable);
        assert_eq!(trim(&q.vendor), "SanDisk");
        assert_eq!(trim(&q.product), "Cruzer Blade");
        assert_eq!(trim(&q.revision), "1.00");
        assert!(parse_inquiry(&buf[..35]).is_none());
    }

    #[test]
    fn trim_handles_padding_and_garbage() {
        assert_eq!(trim(b"        "), "");
        assert_eq!(trim(b"AB\0\0"), "AB");
        assert_eq!(trim(&[b'A', 0xFF, b' ']), "?");
    }

    #[test]
    fn read_capacity_of_the_real_stick() {
        // 60088320 sectors of 512 bytes: last LBA 60088319 = 0x0394_DFFF.
        let buf = [0x03, 0x94, 0xDF, 0xFF, 0x00, 0x00, 0x02, 0x00];
        assert_eq!(
            parse_read_capacity_10(&buf),
            Ok(Capacity { blocks: 60_088_320, block_size: 512 })
        );
    }

    #[test]
    fn read_capacity_edge_cases() {
        assert_eq!(parse_read_capacity_10(&[0; 7]), Err(CapacityError::Short));
        assert_eq!(
            parse_read_capacity_10(&[0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 2, 0]),
            Err(CapacityError::NeedsCapacity16)
        );
        assert_eq!(parse_read_capacity_10(&[0, 0, 0, 1, 0, 0, 0, 0]), Err(CapacityError::ZeroBlockSize));
        // Last LBA 0xFFFF_FFFE must not overflow into a u32.
        assert_eq!(
            parse_read_capacity_10(&[0xFF, 0xFF, 0xFF, 0xFE, 0, 0, 2, 0]).unwrap().blocks,
            0x1_0000_0000 - 1
        );
    }

    #[test]
    fn sense_decoding() {
        let mut buf = [0u8; 18];
        buf[0] = 0xF0; // valid bit + current error
        buf[2] = 0x06;
        buf[12] = 0x28;
        buf[13] = 0x00;
        let s = parse_sense(&buf).unwrap();
        assert_eq!(s, Sense { key: SENSE_UNIT_ATTENTION, asc: 0x28, ascq: 0 });
        assert!(sense_is_transient(s));
        assert_eq!(describe_sense_key(s.key), "UnitAttention");

        buf[2] = SENSE_NOT_READY;
        buf[12] = 0x3A; // medium not present — not transient
        assert!(!sense_is_transient(parse_sense(&buf).unwrap()));
        buf[12] = 0x04; // becoming ready — transient
        assert!(sense_is_transient(parse_sense(&buf).unwrap()));

        buf[0] = 0x72; // descriptor format: not decoded
        assert!(parse_sense(&buf).is_none());
        assert!(parse_sense(&[0x70; 13]).is_none());
    }
}
