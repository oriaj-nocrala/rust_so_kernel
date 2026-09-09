// vfs/src/dirent.rs
//
// Shared getdents64 helpers.
//
// Every directory `FileHandle` in this VFS packs `DirEntry`s into
// `linux_dirent64` records the same way — only *where the entries come
// from* differs, which is why this is two helpers, not one. Before these
// existed, seven directory handles (devfs x2, initramfs, procfs x2, ramfs,
// ext2) each hand-rolled an identical packing loop.

use crate::inode::Inode;
use crate::types::DirEntry;

/// Walk `dir.readdir(offset)` one entry at a time, packing each into `buf`
/// as a `linux_dirent64` record, until either the directory is exhausted
/// or the next entry wouldn't fit. For directory handles backed by a
/// cheap-to-call-repeatedly `Inode::readdir` (devfs, initramfs, procfs) —
/// see `getdents64_from_snapshot` for handles that pre-collect their
/// listing into a `Vec<DirEntry>` at `open()` time instead (ramfs, ext2).
pub fn getdents64_via_readdir(dir: &dyn Inode, offset: &mut u64, buf: &mut [u8]) -> i64 {
    let mut written: usize = 0;
    loop {
        let entry = match dir.readdir(*offset) {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => return e.as_i64(),
        };
        let needed = entry.dirent64_size();
        if written + needed > buf.len() {
            break;
        }
        let next_off = *offset as i64 + 1;
        entry.write_dirent64(next_off, &mut buf[written..written + needed]);
        written += needed;
        *offset += 1;
    }
    written as i64
}

/// Same packing loop as `getdents64_via_readdir`, indexed by position
/// through an already-collected `Vec<DirEntry>` snapshot instead of
/// re-querying `readdir()` per entry.
pub fn getdents64_from_snapshot(entries: &[DirEntry], offset: &mut usize, buf: &mut [u8]) -> i64 {
    let mut written: usize = 0;
    while *offset < entries.len() {
        let entry = &entries[*offset];
        let needed = entry.dirent64_size();
        if written + needed > buf.len() {
            break;
        }
        let next_off = *offset as i64 + 1;
        entry.write_dirent64(next_off, &mut buf[written..written + needed]);
        written += needed;
        *offset += 1;
    }
    written as i64
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::FileHandle;
    use crate::types::{Errno, FileType, OpenFlags, Stat};
    use alloc::boxed::Box;
    use alloc::sync::Arc;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::any::Any;

    /// Build a fixed set of test entries: varying name lengths and types,
    /// so packing math (different `dirent64_size()` per entry) is actually
    /// exercised rather than accidentally uniform.
    fn sample_entries() -> Vec<DirEntry> {
        vec![
            DirEntry::new(1, FileType::Regular, b"a"),
            DirEntry::new(2, FileType::Directory, b"subdir"),
            DirEntry::new(3, FileType::Symlink, b"a-much-longer-name"),
            DirEntry::new(4, FileType::CharDevice, b"dev"),
        ]
    }

    /// A toy directory `Inode` backed by a `Vec<DirEntry>` via `readdir`,
    /// for exercising `getdents64_via_readdir`. `fail_at`, when `Some(n)`,
    /// makes `readdir(n)` return an error instead of the n-th entry.
    struct ToyDir {
        entries: Vec<DirEntry>,
        fail_at: Option<u64>,
    }

    impl Inode for ToyDir {
        fn stat(&self) -> Stat {
            Stat::dir(1)
        }
        fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
            Err(Errno::ENOSYS)
        }
        fn readdir(&self, offset: u64) -> Result<Option<DirEntry>, Errno> {
            if self.fail_at == Some(offset) {
                return Err(Errno::EIO);
            }
            match self.entries.get(offset as usize) {
                Some(e) => Ok(Some(DirEntry::new(e.ino, e.kind, &e.name[..e.name_len]))),
                None => Ok(None),
            }
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn toy_dir() -> ToyDir {
        ToyDir { entries: sample_entries(), fail_at: None }
    }

    // ── Packing correctness (via_readdir) ───────────────────────────────

    #[test]
    fn via_readdir_packs_all_entries_into_a_large_buffer() {
        let dir = toy_dir();
        let entries = sample_entries();
        let expected_total: usize = entries.iter().map(|e| e.dirent64_size()).sum();

        let mut buf = vec![0xFFu8; 4096];
        let mut offset = 0u64;
        let written = getdents64_via_readdir(&dir, &mut offset, &mut buf);

        assert_eq!(written, expected_total as i64);
        assert_eq!(offset, entries.len() as u64);

        // Walk the packed records and verify each starts exactly where the
        // previous one's reclen said it would, with the right d_off,
        // d_type and name.
        let mut pos = 0usize;
        for (i, e) in entries.iter().enumerate() {
            let reclen = u16::from_le_bytes(buf[pos + 16..pos + 18].try_into().unwrap()) as usize;
            let d_off = i64::from_le_bytes(buf[pos + 8..pos + 16].try_into().unwrap());
            let d_type = buf[pos + 18];
            let name_len = e.name_len;
            let name = &buf[pos + 19..pos + 19 + name_len];

            assert_eq!(d_off, (i + 1) as i64, "entry {i} d_off");
            assert_eq!(d_type, e.kind.as_dt_type(), "entry {i} d_type");
            assert_eq!(name, &e.name[..name_len], "entry {i} name");
            assert_eq!(reclen, e.dirent64_size(), "entry {i} reclen");

            pos += reclen;
        }
        assert_eq!(pos, expected_total);
    }

    #[test]
    fn from_snapshot_packs_all_entries_into_a_large_buffer() {
        let entries = sample_entries();
        let expected_total: usize = entries.iter().map(|e| e.dirent64_size()).sum();

        let mut buf = vec![0xFFu8; 4096];
        let mut offset = 0usize;
        let written = getdents64_from_snapshot(&entries, &mut offset, &mut buf);

        assert_eq!(written, expected_total as i64);
        assert_eq!(offset, entries.len());
    }

    // ── Partial buffer + resumable offset ───────────────────────────────

    #[test]
    fn via_readdir_partial_buffer_resumes_from_saved_offset() {
        let dir = toy_dir();
        let entries = sample_entries();

        // A buffer that fits exactly the first two entries (and not a
        // byte more), so the third entry doesn't fit.
        let first_two: usize = entries[0].dirent64_size() + entries[1].dirent64_size();
        let mut buf = vec![0u8; first_two];
        let mut offset = 0u64;
        let written = getdents64_via_readdir(&dir, &mut offset, &mut buf);

        assert_eq!(written, first_two as i64);
        assert_eq!(offset, 2);

        // A second call with the same `offset` (now 2) and a fresh large
        // buffer continues exactly where the first left off — entries 2
        // and 3, not repeating 0/1 and not skipping any.
        let mut buf2 = vec![0u8; 4096];
        let written2 = getdents64_via_readdir(&dir, &mut offset, &mut buf2);
        let rest: usize = entries[2].dirent64_size() + entries[3].dirent64_size();
        assert_eq!(written2, rest as i64);
        assert_eq!(offset, 4);

        // ino of the first record in the second call must be entry[2]'s,
        // proving no entry was repeated or skipped.
        let ino = u64::from_le_bytes(buf2[0..8].try_into().unwrap());
        assert_eq!(ino, entries[2].ino);
    }

    #[test]
    fn from_snapshot_partial_buffer_resumes_from_saved_offset() {
        let entries = sample_entries();
        let first_two: usize = entries[0].dirent64_size() + entries[1].dirent64_size();
        let mut buf = vec![0u8; first_two];
        let mut offset = 0usize;
        let written = getdents64_from_snapshot(&entries, &mut offset, &mut buf);

        assert_eq!(written, first_two as i64);
        assert_eq!(offset, 2);

        let mut buf2 = vec![0u8; 4096];
        let written2 = getdents64_from_snapshot(&entries, &mut offset, &mut buf2);
        let rest: usize = entries[2].dirent64_size() + entries[3].dirent64_size();
        assert_eq!(written2, rest as i64);
        assert_eq!(offset, 4);

        let ino = u64::from_le_bytes(buf2[0..8].try_into().unwrap());
        assert_eq!(ino, entries[2].ino);
    }

    // ── Exhausted directory ──────────────────────────────────────────────

    #[test]
    fn via_readdir_exhausted_returns_zero_and_does_not_touch_buffer() {
        let dir = toy_dir();
        let mut offset = sample_entries().len() as u64; // already past the end
        let mut buf = vec![0xABu8; 64];
        let written = getdents64_via_readdir(&dir, &mut offset, &mut buf);
        assert_eq!(written, 0);
        assert!(buf.iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn from_snapshot_exhausted_returns_zero_and_does_not_touch_buffer() {
        let entries = sample_entries();
        let mut offset = entries.len();
        let mut buf = vec![0xABu8; 64];
        let written = getdents64_from_snapshot(&entries, &mut offset, &mut buf);
        assert_eq!(written, 0);
        assert!(buf.iter().all(|&b| b == 0xAB));
    }

    // ── Buffer too small even for the first entry ───────────────────────
    //
    // Read from the code (not assumed): `written` starts at 0, and the
    // loop's very first check is `if written + needed > buf.len() { break }`
    // — before anything is ever written into `buf`. So a buffer smaller
    // than even the first entry's `dirent64_size()` makes the loop break
    // immediately on iteration 1, returning 0 and leaving `offset`
    // unchanged (untouched, still 0) — same as "directory exhausted" from
    // the caller's point of view, even though there was more to give.

    #[test]
    fn via_readdir_buffer_too_small_for_first_entry_returns_zero() {
        let dir = toy_dir();
        let smallest = sample_entries()[0].dirent64_size();
        let mut buf = vec![0xCDu8; smallest - 1]; // one byte too small
        let mut offset = 0u64;
        let written = getdents64_via_readdir(&dir, &mut offset, &mut buf);
        assert_eq!(written, 0);
        assert_eq!(offset, 0, "offset must not advance when nothing was packed");
        assert!(buf.iter().all(|&b| b == 0xCD), "buffer must be untouched");
    }

    #[test]
    fn from_snapshot_buffer_too_small_for_first_entry_returns_zero() {
        let entries = sample_entries();
        let smallest = entries[0].dirent64_size();
        let mut buf = vec![0xCDu8; smallest - 1];
        let mut offset = 0usize;
        let written = getdents64_from_snapshot(&entries, &mut offset, &mut buf);
        assert_eq!(written, 0);
        assert_eq!(offset, 0);
        assert!(buf.iter().all(|&b| b == 0xCD));
    }

    // ── readdir() error propagation (via_readdir only) ──────────────────

    #[test]
    fn via_readdir_propagates_readdir_error_as_negative_errno() {
        // Entries 0 and 1 pack fine, then readdir(2) fails. The helper
        // must return the negative errno directly, NOT the bytes already
        // written for entries 0/1 — a real behavioral difference from
        // `getdents64_from_snapshot`, which can never fail this way since
        // a plain slice index can't error.
        let dir = ToyDir { entries: sample_entries(), fail_at: Some(2) };
        let mut buf = vec![0u8; 4096];
        let mut offset = 0u64;
        let written = getdents64_via_readdir(&dir, &mut offset, &mut buf);
        assert_eq!(written, Errno::EIO.as_i64());
        assert!(written < 0);
    }

    // ── Both helpers produce identical bytes for the same entries ───────

    #[test]
    fn via_readdir_and_from_snapshot_produce_identical_bytes() {
        let entries = sample_entries();
        let dir = ToyDir { entries: sample_entries(), fail_at: None };

        let mut buf_a = vec![0u8; 4096];
        let mut offset_a = 0u64;
        let written_a = getdents64_via_readdir(&dir, &mut offset_a, &mut buf_a);

        let mut buf_b = vec![0u8; 4096];
        let mut offset_b = 0usize;
        let written_b = getdents64_from_snapshot(&entries, &mut offset_b, &mut buf_b);

        assert_eq!(written_a, written_b);
        assert_eq!(offset_a as usize, offset_b);
        assert_eq!(buf_a[..written_a as usize], buf_b[..written_b as usize]);
    }

    // Sanity: `Arc<dyn Inode>` coercion compiles for a ToyDir (exercised
    // implicitly by other kernel code, not by this helper directly, but
    // pins the trait's dyn-compatibility while it lives in this module).
    #[test]
    fn toy_dir_is_dyn_compatible() {
        let dir: Arc<dyn Inode> = Arc::new(toy_dir());
        assert_eq!(dir.file_type(), FileType::Directory);
    }
}
