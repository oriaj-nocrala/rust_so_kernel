//! ACPI table parsing: RSDP -> (XSDT preferred, RSDT fallback) -> MADT.
//!
//! Pure logic — moved here (out of `kernel/src/acpi.rs`) so it can be unit
//! tested on the host with `cargo test`, no QEMU required. Everything reads
//! through the injected `&dyn PhysMem` seam into local, safely-sized
//! buffers, then decodes fields with `from_le_bytes` — no raw pointers, no
//! `#[repr(C, packed)]`, no `read_unaligned`. This is both simpler and
//! trivially host-safe compared to the kernel's original pointer-based
//! implementation.
//!
//! Anti-OOB discipline carried over unchanged: every variable-length list
//! (XSDT/RSDT entry array, MADT interrupt-controller-structure list) is
//! walked with an explicit bounds check against the enclosing table's
//! declared length *before* any byte of a sub-structure is trusted — this
//! kernel has already shipped one real out-of-bounds bug (the Quake WAV
//! parser trusting a chunk size before validating it against remaining file
//! length) from skipping exactly this. The malformed-table tests below
//! exist specifically to keep exercising those guards.
//!
//! The kernel adapter (`kernel/src/acpi.rs`) is responsible for hardware
//! access (reading physical memory via the bootloader's offset), logging,
//! and the `spin::Once` global — this module only parses and returns
//! `Result`.

use alloc::vec::Vec;

use crate::PhysMem;

// ── Public data model ───────────────────────────────────────────────────────

/// One enabled CPU, as reported by a MADT Processor Local APIC entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuInfo {
    pub processor_id: u8,
    pub apic_id: u8,
}

/// One I/O APIC, as reported by a MADT I/O APIC entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoApic {
    pub id: u8,
    pub address: u32,
    pub gsi_base: u32,
}

/// One interrupt source override (legacy ISA IRQ -> Global System
/// Interrupt remap), as reported by a MADT Interrupt Source Override entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Iso {
    pub bus: u8,
    pub source: u8,
    pub gsi: u32,
    pub flags: u16,
}

/// Everything this parser extracts from the MADT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpiTopology {
    pub local_apic_addr: u64,
    pub cpus: Vec<CpuInfo>,
    pub io_apics: Vec<IoApic>,
    pub overrides: Vec<Iso>,
}

/// Reasons `parse()` can fail to produce a topology. Deliberately specific
/// rather than a single "parse failed" bool — the kernel adapter logs which
/// one happened, same detail level the original inline implementation
/// logged at each early-return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpiError {
    /// RSDP signature didn't match `"RSD PTR "`.
    BadSignature,
    /// A checksum (RSDP first-20-bytes, RSDP extended, XSDT/RSDT, or MADT)
    /// didn't sum to 0 mod 256.
    BadChecksum,
    /// Neither a usable XSDT nor RSDT address was present.
    NoRootTable,
    /// The root table's entry list contained no MADT ("APIC") table.
    NoMadt,
}

// ── Layout constants ─────────────────────────────────────────────────────────

/// Length of the 36-byte header that prefixes every ACPI system description
/// table (SDT), including XSDT/RSDT/MADT themselves.
const SDT_HEADER_LEN: usize = 36;

const MADT_TYPE_LOCAL_APIC: u8 = 0;
const MADT_TYPE_IO_APIC: u8 = 1;
const MADT_TYPE_INTERRUPT_SOURCE_OVERRIDE: u8 = 2;
const MADT_TYPE_LOCAL_APIC_ADDRESS_OVERRIDE: u8 = 5;

// ── Low-level seam helpers ───────────────────────────────────────────────────

/// Reads exactly `N` bytes at physical address `pa` through the seam into a
/// local, stack-owned buffer.
fn read_bytes<const N: usize>(mem: &dyn PhysMem, pa: u64) -> [u8; N] {
    let mut buf = [0u8; N];
    mem.read(pa, &mut buf);
    buf
}

/// ACPI checksum rule: valid iff the sum of every byte over `len`, taken
/// physically starting at `pa`, is 0 mod 256.
fn checksum_ok(mem: &dyn PhysMem, pa: u64, len: usize) -> bool {
    let mut buf = alloc::vec![0u8; len];
    mem.read(pa, &mut buf);
    buf.iter().fold(0u8, |acc, &b| acc.wrapping_add(b)) == 0
}

// ── MADT interrupt-controller-structure walk ────────────────────────────────

/// Walks the MADT's variable-length interrupt-controller-structure list
/// (starting right after the fixed 44-byte MADT header: 36-byte SDT header
/// + 4-byte Local APIC Address + 4-byte Flags) and populates `topo`.
///
/// `madt_pa` / `madt_len` describe the whole table (including its SDT
/// header), already checksum-validated by the caller.
fn parse_madt(mem: &dyn PhysMem, madt_pa: u64, madt_len: usize, topo: &mut AcpiTopology) {
    const MADT_ENTRIES_START: usize = 44;

    let mut offset = MADT_ENTRIES_START;
    while offset + 2 <= madt_len {
        let hdr = read_bytes::<2>(mem, madt_pa + offset as u64);
        let entry_type = hdr[0];
        let entry_len = hdr[1] as usize;

        // Anti-OOB guard: a zero (or 1-byte) length would spin forever or
        // read out of its own header; a length that would run past the
        // table's own declared size is corrupt data — abort the walk
        // rather than trust it (same lesson as the Quake WAV parser's
        // unvalidated chunk-size bug).
        if entry_len < 2 || offset + entry_len > madt_len {
            break;
        }

        let mut buf = alloc::vec![0u8; entry_len];
        mem.read(madt_pa + offset as u64, &mut buf);

        match entry_type {
            MADT_TYPE_LOCAL_APIC if entry_len >= 8 => {
                let processor_id = buf[2];
                let apic_id = buf[3];
                let flags = u32::from_le_bytes(buf[4..8].try_into().unwrap());
                if flags & 1 != 0 {
                    topo.cpus.push(CpuInfo { processor_id, apic_id });
                }
            }
            MADT_TYPE_IO_APIC if entry_len >= 12 => {
                let id = buf[2];
                let address = u32::from_le_bytes(buf[4..8].try_into().unwrap());
                let gsi_base = u32::from_le_bytes(buf[8..12].try_into().unwrap());
                topo.io_apics.push(IoApic { id, address, gsi_base });
            }
            MADT_TYPE_INTERRUPT_SOURCE_OVERRIDE if entry_len >= 10 => {
                let bus = buf[2];
                let source = buf[3];
                let gsi = u32::from_le_bytes(buf[4..8].try_into().unwrap());
                let flags = u16::from_le_bytes(buf[8..10].try_into().unwrap());
                topo.overrides.push(Iso { bus, source, gsi, flags });
            }
            MADT_TYPE_LOCAL_APIC_ADDRESS_OVERRIDE if entry_len >= 12 => {
                topo.local_apic_addr = u64::from_le_bytes(buf[4..12].try_into().unwrap());
            }
            _ => {
                // Every other type (x2APIC, NMI sources, etc.) is ignored —
                // nothing here needs them. Just skip past via entry_len,
                // already validated above.
            }
        }

        offset += entry_len;
    }
}

/// Scans one root table's (XSDT or RSDT) entry array for a table whose
/// signature is `b"APIC"` (the MADT), validating each candidate's checksum
/// before trusting it. `entry_size` is 8 for XSDT, 4 for RSDT.
fn find_madt(mem: &dyn PhysMem, root_pa: u64, root_len: usize, entry_size: usize) -> Option<(u64, usize)> {
    if root_len < SDT_HEADER_LEN {
        return None;
    }
    let entries_bytes = root_len - SDT_HEADER_LEN;
    let count = entries_bytes / entry_size;

    for i in 0..count {
        let entry_offset = SDT_HEADER_LEN + i * entry_size;
        // Anti-OOB guard, mirroring parse_madt's discipline.
        if entry_offset + entry_size > root_len {
            break;
        }

        let table_pa: u64 = if entry_size == 8 {
            u64::from_le_bytes(read_bytes::<8>(mem, root_pa + entry_offset as u64))
        } else {
            u32::from_le_bytes(read_bytes::<4>(mem, root_pa + entry_offset as u64)) as u64
        };
        if table_pa == 0 {
            continue;
        }

        let hdr = read_bytes::<SDT_HEADER_LEN>(mem, table_pa);
        let sig = &hdr[0..4];
        let len = u32::from_le_bytes(hdr[4..8].try_into().unwrap()) as usize;

        if sig != b"APIC" {
            continue;
        }
        if len < SDT_HEADER_LEN || !checksum_ok(mem, table_pa, len) {
            continue;
        }
        return Some((table_pa, len));
    }
    None
}

// ── Public entry point ───────────────────────────────────────────────────────

/// Locates the RSDP at `rsdp_pa`, walks to the XSDT (preferred) or RSDT,
/// finds the MADT, and extracts interrupt topology from it. Pure parsing —
/// does no logging and touches no global state; the kernel adapter is
/// responsible for both.
pub fn parse(mem: &dyn PhysMem, rsdp_pa: u64) -> Result<AcpiTopology, AcpiError> {
    // RSDP, ACPI 2.0+ layout (36 bytes). On ACPI 1.0 (revision == 0) only
    // the first 20 bytes (up to and including rsdt_address) are
    // valid/present in memory — the fields past that must not be trusted
    // in that case, which is exactly what `use_xsdt` below guards against.
    let rsdp = read_bytes::<36>(mem, rsdp_pa);

    if &rsdp[0..8] != b"RSD PTR " {
        return Err(AcpiError::BadSignature);
    }
    // ACPI 1.0 checksum covers the first 20 bytes regardless of revision.
    if !checksum_ok(mem, rsdp_pa, 20) {
        return Err(AcpiError::BadChecksum);
    }

    let revision = rsdp[15];
    let rsdt_address = u32::from_le_bytes(rsdp[16..20].try_into().unwrap());
    let length = u32::from_le_bytes(rsdp[20..24].try_into().unwrap());
    let xsdt_address = u64::from_le_bytes(rsdp[24..32].try_into().unwrap());

    let use_xsdt = revision >= 2 && xsdt_address != 0;

    let (root_pa, entry_size) = if use_xsdt {
        // Extended checksum covers the whole extended structure
        // (`rsdp.length` bytes), only meaningful/present for rev >= 2.
        if !checksum_ok(mem, rsdp_pa, length as usize) {
            return Err(AcpiError::BadChecksum);
        }
        (xsdt_address, 8usize)
    } else {
        if rsdt_address == 0 {
            return Err(AcpiError::NoRootTable);
        }
        (rsdt_address as u64, 4usize)
    };

    let root_hdr = read_bytes::<SDT_HEADER_LEN>(mem, root_pa);
    let root_len = u32::from_le_bytes(root_hdr[4..8].try_into().unwrap()) as usize;
    if root_len < SDT_HEADER_LEN || !checksum_ok(mem, root_pa, root_len) {
        return Err(AcpiError::BadChecksum);
    }

    let Some((madt_pa, madt_len)) = find_madt(mem, root_pa, root_len, entry_size) else {
        return Err(AcpiError::NoMadt);
    };

    // MADT's own fixed fields, right after its 36-byte SDT header: 4-byte
    // Local APIC Address, then 4-byte Flags (unused here).
    let local_apic_addr = u32::from_le_bytes(read_bytes::<4>(mem, madt_pa + 36)) as u64;

    let mut topo = AcpiTopology {
        local_apic_addr,
        cpus: Vec::new(),
        io_apics: Vec::new(),
        overrides: Vec::new(),
    };
    parse_madt(mem, madt_pa, madt_len, &mut topo);
    Ok(topo)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec as AVec;

    /// A flat, `Vec<u8>`-backed physical address space: physical address
    /// `pa` maps directly to byte index `pa` in the buffer. Good enough for
    /// synthetic ACPI images built at small, hand-picked offsets.
    struct VecMem {
        data: AVec<u8>,
    }

    impl PhysMem for VecMem {
        fn read(&self, pa: u64, buf: &mut [u8]) {
            let start = pa as usize;
            let end = start + buf.len();
            buf.copy_from_slice(&self.data[start..end]);
        }
    }

    /// Recomputes and writes the ACPI checksum byte at `checksum_offset` so
    /// that the sum of every byte in `data[start..start+len]` is 0 mod 256.
    /// `checksum_offset` must fall inside `start..start+len`.
    fn fix_checksum(data: &mut [u8], start: usize, len: usize, checksum_offset: usize) {
        data[checksum_offset] = 0;
        let partial_sum: u8 = data[start..start + len].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        data[checksum_offset] = 0u8.wrapping_sub(partial_sum);
    }

    const RSDP_PA: usize = 0x1000;
    const XSDT_PA: usize = 0x2000;
    const MADT_PA: usize = 0x3000;

    /// Builds a well-formed RSDP (rev 2) at `RSDP_PA` pointing at an XSDT at
    /// `XSDT_PA` (single entry) pointing at a MADT at `MADT_PA`, with
    /// correct checksums throughout. Callers can further mutate the
    /// returned buffer (and must re-fix checksums if they touch covered
    /// bytes) to build malformed variants.
    fn build_valid_image() -> AVec<u8> {
        let mut data = alloc::vec![0u8; 0x4000];

        // ── RSDP (36 bytes) ──────────────────────────────────────────
        data[RSDP_PA..RSDP_PA + 8].copy_from_slice(b"RSD PTR ");
        // byte 8: checksum, fixed up below
        data[RSDP_PA + 9..RSDP_PA + 15].copy_from_slice(b"TESTOE");
        data[RSDP_PA + 15] = 2; // revision 2 (ACPI 2.0+)
        data[RSDP_PA + 16..RSDP_PA + 20].copy_from_slice(&0u32.to_le_bytes()); // rsdt_address (unused, rev>=2)
        data[RSDP_PA + 20..RSDP_PA + 24].copy_from_slice(&36u32.to_le_bytes()); // length
        data[RSDP_PA + 24..RSDP_PA + 32].copy_from_slice(&(XSDT_PA as u64).to_le_bytes());
        // byte 32: extended checksum, fixed up below; bytes 33..36 reserved = 0

        fix_checksum(&mut data, RSDP_PA, 20, RSDP_PA + 8);
        fix_checksum(&mut data, RSDP_PA, 36, RSDP_PA + 32);

        // ── XSDT (36-byte header + 1 entry of 8 bytes = 44) ─────────
        let xsdt_len: u32 = 44;
        data[XSDT_PA..XSDT_PA + 4].copy_from_slice(b"XSDT");
        data[XSDT_PA + 4..XSDT_PA + 8].copy_from_slice(&xsdt_len.to_le_bytes());
        data[XSDT_PA + 36..XSDT_PA + 44].copy_from_slice(&(MADT_PA as u64).to_le_bytes());
        fix_checksum(&mut data, XSDT_PA, xsdt_len as usize, XSDT_PA + 9);

        // ── MADT: 44-byte header + 3 entries (8 + 12 + 10 = 30) ─────
        let madt_len: u32 = 44 + 8 + 12 + 10;
        data[MADT_PA..MADT_PA + 4].copy_from_slice(b"APIC");
        data[MADT_PA + 4..MADT_PA + 8].copy_from_slice(&madt_len.to_le_bytes());
        data[MADT_PA + 36..MADT_PA + 40].copy_from_slice(&0xFEE00000u32.to_le_bytes()); // Local APIC Address
        data[MADT_PA + 40..MADT_PA + 44].copy_from_slice(&0u32.to_le_bytes()); // Flags

        let mut off = MADT_PA + 44;

        // Entry: Processor Local APIC (type 0, len 8) — enabled.
        data[off] = 0;
        data[off + 1] = 8;
        data[off + 2] = 1; // processor_id
        data[off + 3] = 1; // apic_id
        data[off + 4..off + 8].copy_from_slice(&1u32.to_le_bytes()); // flags: enabled
        off += 8;

        // Entry: I/O APIC (type 1, len 12).
        data[off] = 1;
        data[off + 1] = 12;
        data[off + 2] = 0; // id
        data[off + 3] = 0; // reserved
        data[off + 4..off + 8].copy_from_slice(&0xFEC00000u32.to_le_bytes()); // address
        data[off + 8..off + 12].copy_from_slice(&0u32.to_le_bytes()); // gsi_base
        off += 12;

        // Entry: Interrupt Source Override (type 2, len 10) — IRQ0 -> GSI2.
        data[off] = 2;
        data[off + 1] = 10;
        data[off + 2] = 0; // bus
        data[off + 3] = 0; // source (IRQ0)
        data[off + 4..off + 8].copy_from_slice(&2u32.to_le_bytes()); // gsi
        data[off + 8..off + 10].copy_from_slice(&0u16.to_le_bytes()); // flags
        off += 10;
        debug_assert_eq!(off, MADT_PA + madt_len as usize);

        fix_checksum(&mut data, MADT_PA, madt_len as usize, MADT_PA + 9);

        data
    }

    #[test]
    fn valid_image_parses_expected_topology() {
        let data = build_valid_image();
        let mem = VecMem { data };

        let topo = parse(&mem, RSDP_PA as u64).expect("valid image should parse");

        assert_eq!(topo.local_apic_addr, 0xFEE00000);
        assert_eq!(topo.cpus, alloc::vec![CpuInfo { processor_id: 1, apic_id: 1 }]);
        assert_eq!(
            topo.io_apics,
            alloc::vec![IoApic { id: 0, address: 0xFEC00000, gsi_base: 0 }]
        );
        assert_eq!(
            topo.overrides,
            alloc::vec![Iso { bus: 0, source: 0, gsi: 2, flags: 0 }]
        );
    }

    #[test]
    fn bad_rsdp_signature_is_rejected() {
        let mut data = build_valid_image();
        data[RSDP_PA] = b'X'; // corrupt "RSD PTR " -> "XSD PTR "
        let mem = VecMem { data };

        assert_eq!(parse(&mem, RSDP_PA as u64), Err(AcpiError::BadSignature));
    }

    #[test]
    fn bad_rsdp_checksum_is_rejected() {
        let mut data = build_valid_image();
        // Flip a byte inside the checksummed region without fixing the
        // checksum back up.
        data[RSDP_PA + 9] ^= 0xFF;
        let mem = VecMem { data };

        assert_eq!(parse(&mem, RSDP_PA as u64), Err(AcpiError::BadChecksum));
    }

    #[test]
    fn xsdt_with_no_madt_is_reported() {
        let mut data = build_valid_image();
        // Retarget the XSDT's one entry to point at the XSDT table itself
        // (self-referential, but harmless — find_madt only reads a
        // header-sized prefix) instead of the real MADT, and relabel that
        // header's signature to something that isn't "APIC". find_madt then
        // walks one real, valid-checksum, non-APIC entry and finds nothing.
        data[XSDT_PA + 36..XSDT_PA + 44].copy_from_slice(&(XSDT_PA as u64).to_le_bytes());
        data[XSDT_PA..XSDT_PA + 4].copy_from_slice(b"FACP");
        let xsdt_len = u32::from_le_bytes(data[XSDT_PA + 4..XSDT_PA + 8].try_into().unwrap());
        fix_checksum(&mut data, XSDT_PA, xsdt_len as usize, XSDT_PA + 9);

        let mem = VecMem { data };
        assert_eq!(parse(&mem, RSDP_PA as u64), Err(AcpiError::NoMadt));
    }

    #[test]
    fn madt_entry_with_zero_length_terminates_walk_cleanly() {
        let mut data = build_valid_image();
        // Corrupt the *first* MADT entry's length field to 0. The walk
        // must abort immediately instead of looping forever, and must not
        // read any of the (still well-formed) entries after it.
        let first_entry_off = MADT_PA + 44;
        data[first_entry_off + 1] = 0; // length = 0
        // Shrink madt_len is not required — the point is the *walk* stops,
        // not that parse() fails; checksum must still be fixed since we
        // changed a covered byte.
        let madt_len = u32::from_le_bytes(data[MADT_PA + 4..MADT_PA + 8].try_into().unwrap());
        fix_checksum(&mut data, MADT_PA, madt_len as usize, MADT_PA + 9);

        let mem = VecMem { data };
        // Must terminate (this test would hang forever on a real infinite
        // loop) and must not have picked up any of the entries after the
        // corrupt one, since the walk aborts at first bad entry.
        let topo = parse(&mem, RSDP_PA as u64).expect("outer tables are still valid");
        assert!(topo.cpus.is_empty());
        assert!(topo.io_apics.is_empty());
        assert!(topo.overrides.is_empty());
    }

    #[test]
    fn madt_entry_length_past_table_end_terminates_walk_cleanly() {
        let mut data = build_valid_image();
        // Corrupt the *first* MADT entry's length to something that runs
        // past the table's declared length — must not read out of bounds,
        // must stop cleanly instead.
        let first_entry_off = MADT_PA + 44;
        data[first_entry_off + 1] = 0xFF; // length = 255, way past table end
        let madt_len = u32::from_le_bytes(data[MADT_PA + 4..MADT_PA + 8].try_into().unwrap());
        fix_checksum(&mut data, MADT_PA, madt_len as usize, MADT_PA + 9);

        let mem = VecMem { data };
        let topo = parse(&mem, RSDP_PA as u64).expect("outer tables are still valid");
        assert!(topo.cpus.is_empty());
        assert!(topo.io_apics.is_empty());
        assert!(topo.overrides.is_empty());
    }
}
