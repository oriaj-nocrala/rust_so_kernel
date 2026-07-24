// ext2/src/dirent.rs
//
// Pure `ext2_dir_entry_2` record format: length rounding, serialization,
// and parsing of a single record. Moved out of `kernel::fs::ext2`'s
// `dirent_len`/`write_dirent` free functions (used directly by
// `add_dir_entry`/`mkdir`/the test image builders) — migration step 1, no
// behavior change.
//
// Directory *operations* (walking a directory's data blocks to list,
// insert, or remove entries — `read_dir_entries`/`add_dir_entry`/
// `remove_dir_entry`/`set_dotdot`) are migration step 4 and deliberately
// have NOT moved: they stay in `kernel::fs::ext2`, still with their own
// inline record parsing (unchanged, not perturbed by this module's
// existence). `ParsedDirent::parse` below exists so this crate can test
// the record format in isolation now; step 4 can adopt it later to
// de-duplicate that inline parsing, but that's out of scope here.
//
// This module deliberately works in terms of the raw on-disk `file_type`
// byte (1=regular, 2=dir, 3=block dev, 4=char dev, 7=symlink), not the
// kernel's `fs::types::FileType` enum — this crate doesn't depend on the
// kernel, so that mapping (`ext2_file_type_to_vfs`/`vfs_file_type_to_ext2`)
// stays in the kernel adapter.

/// On-disk `ext2_dir_entry_2` record length for a `name_len`-byte name,
/// rounded up to 4-byte alignment (`8 + name_len`, then rounded).
pub fn dirent_len(name_len: usize) -> usize {
    (8 + name_len + 3) & !3
}

/// Serialize one directory entry into `buf` (must be exactly `rec_len`
/// bytes — the caller decides how much slack this entry claims).
pub fn write_dirent(buf: &mut [u8], ino: u32, rec_len: u16, name: &str, file_type: u8) {
    buf[0..4].copy_from_slice(&ino.to_le_bytes());
    buf[4..6].copy_from_slice(&rec_len.to_le_bytes());
    buf[6] = name.len() as u8;
    buf[7] = file_type;
    buf[8..8 + name.len()].copy_from_slice(name.as_bytes());
}

/// One directory entry record decoded from `buf` at byte offset `off`.
/// `name` borrows straight out of `buf` — no allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedDirent<'a> {
    pub ino: u32,
    pub rec_len: u16,
    pub file_type: u8,
    pub name: &'a [u8],
}

impl<'a> ParsedDirent<'a> {
    /// Returns `None` if there isn't a full 8-byte header left in `buf` at
    /// `off`, `rec_len` is corrupt (`< 8`, the same "stop rather than loop
    /// forever" guard `read_dir_entries` applies inline), or the declared
    /// `name_len` would run past `buf`'s end.
    pub fn parse(buf: &'a [u8], off: usize) -> Option<Self> {
        if off + 8 > buf.len() {
            return None;
        }
        let ino = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
        let rec_len = u16::from_le_bytes(buf[off + 4..off + 6].try_into().unwrap());
        if rec_len < 8 {
            return None;
        }
        let name_len = buf[off + 6] as usize;
        let file_type = buf[off + 7];
        if off + 8 + name_len > buf.len() {
            return None;
        }
        Some(Self { ino, rec_len, file_type, name: &buf[off + 8..off + 8 + name_len] })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirent_len_rounds_up_to_4_bytes() {
        assert_eq!(dirent_len(0), 8); // 8 + 0 = 8, already aligned
        assert_eq!(dirent_len(1), 12); // 8 + 1 = 9 -> 12
        assert_eq!(dirent_len(2), 12); // 8 + 2 = 10 -> 12
        assert_eq!(dirent_len(4), 12); // 8 + 4 = 12, already aligned
        assert_eq!(dirent_len(5), 16); // 8 + 5 = 13 -> 16
    }

    #[test]
    fn write_then_parse_round_trips() {
        let mut buf = [0u8; 32];
        let rec_len = dirent_len(5) as u16;
        write_dirent(&mut buf, 42, rec_len, "hello", 1);
        let parsed = ParsedDirent::parse(&buf, 0).expect("parse");
        assert_eq!(parsed.ino, 42);
        assert_eq!(parsed.rec_len, rec_len);
        assert_eq!(parsed.file_type, 1);
        assert_eq!(parsed.name, b"hello");
    }

    #[test]
    fn parse_at_nonzero_offset() {
        let mut buf = [0u8; 64];
        let first_len = dirent_len(1) as u16;
        write_dirent(&mut buf[0..first_len as usize], 2, first_len, ".", 2);
        let second_off = first_len as usize;
        let second_len = dirent_len(2) as u16;
        write_dirent(&mut buf[second_off..second_off + second_len as usize], 2, second_len, "..", 2);

        let first = ParsedDirent::parse(&buf, 0).expect("first");
        assert_eq!(first.name, b".");
        let second = ParsedDirent::parse(&buf, second_off).expect("second");
        assert_eq!(second.name, b"..");
    }

    #[test]
    fn parse_rejects_truncated_header() {
        let buf = [0u8; 4]; // fewer than 8 bytes
        assert_eq!(ParsedDirent::parse(&buf, 0), None);
    }

    #[test]
    fn parse_rejects_corrupt_rec_len() {
        let mut buf = [0u8; 32];
        buf[4..6].copy_from_slice(&4u16.to_le_bytes()); // < 8, corrupt
        assert_eq!(ParsedDirent::parse(&buf, 0), None);
    }

    #[test]
    fn parse_rejects_name_len_past_buffer_end() {
        let mut buf = [0u8; 12];
        buf[4..6].copy_from_slice(&12u16.to_le_bytes());
        buf[6] = 200; // name_len way past the 12-byte buffer
        assert_eq!(ParsedDirent::parse(&buf, 0), None);
    }

    #[test]
    fn deleted_slot_parses_with_zero_inode() {
        let mut buf = [0u8; 16];
        // rec_len 16, inode 0 (deleted/reusable slot), no name.
        buf[4..6].copy_from_slice(&16u16.to_le_bytes());
        let parsed = ParsedDirent::parse(&buf, 0).expect("parse");
        assert_eq!(parsed.ino, 0);
        assert_eq!(parsed.rec_len, 16);
        assert!(parsed.name.is_empty());
    }
}
