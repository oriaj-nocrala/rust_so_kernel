// ext2/src/dir.rs
//
// Directory operations (list/insert/remove a directory entry, rewrite
// ".."), plus the symlink fast/slow target read+write — migration step 4
// (`docs/fs/ext2-extraction-plan.md`). Moved verbatim out of
// `kernel::fs::ext2::Ext2Fs` — same on-disk format, same write ordering
// ("allocate & write content, then link"), same error conditions. See the
// crate doc comment for why this crate doesn't know about `fs::types`.
//
// This module deliberately works in terms of the raw on-disk `file_type`
// byte (1=regular, 2=dir, 3=block dev, 4=char dev, 7=symlink), never the
// kernel's `fs::types::FileType` enum — the `ext2_file_type_to_vfs`/
// `vfs_file_type_to_ext2` mapping stays in the kernel adapter (see
// `dirent.rs`'s module doc comment, which already established this split
// for the pure record format `read_dir_entries`/`add_dir_entry`/
// `remove_dir_entry` now also follow).
//
// What is deliberately NOT here (still in `kernel::fs::ext2`, unmigrated):
// `mount`'s own repair passes (`reconcile_free_counts`/`reclaim_orphans`,
// migration step 5) and every VFS-facing method on `Ext2Inode` (`create`/
// `mkdir`/`unlink`/`rmdir`/`take_child`/`insert_child`/`symlink`'s own
// inode-alloc-and-mode-setup half) — those still decide *when* to call the
// functions here, and own the `EXT2_LOCK` critical sections, but the
// byte-level directory-data manipulation itself now lives here.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::dirent::{dirent_len, write_dirent, ParsedDirent};
use crate::error::Ext2Error;
use crate::inode::RawInode;
use crate::volume::Ext2Core;

/// One directory entry as read back by [`Ext2Core::read_dir_entries`] —
/// `file_type` is the raw on-disk byte (see this module's doc comment),
/// not a VFS-level type. The kernel adapter converts it to `fs::types::
/// FileType` at the boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub ino: u32,
    pub file_type: u8,
    pub name: String,
}

impl Ext2Core {
    // ── Directory entries ────────────────────────────────────────────────

    /// Parse every directory entry out of `raw`'s data blocks (direct +
    /// indirect, same limit as file reads). Synthetic "."/".." entries are
    /// never included — callers that need them (directory listings) add
    /// their own, same convention the kernel adapter's `open()`/`readdir()`
    /// already use.
    pub fn read_dir_entries(&self, raw: &RawInode) -> Result<Vec<DirEntry>, Ext2Error> {
        let mut entries = Vec::new();
        let bs = self.sb.block_size;
        let num_blocks = (raw.size() + bs as u64 - 1) / bs as u64;

        for block_index in 0..num_blocks as u32 {
            let Some(block_num) = self.block_for_index(raw, block_index)? else { continue };
            let buf = self.block_vec(block_num)?;
            let mut off = 0usize;
            while off + 8 <= buf.len() {
                // Read `rec_len` directly first (not via `ParsedDirent::
                // parse`, which would also reject a corrupt/oversized
                // `name_len` by returning `None` — that must still advance
                // `off` and keep scanning below, not stop the whole loop,
                // exactly like before this used `ParsedDirent`).
                let rec_len = u16::from_le_bytes(buf[off + 4..off + 6].try_into().unwrap()) as usize;
                if rec_len < 8 {
                    break; // corrupt — stop rather than loop forever
                }
                if let Some(entry) = ParsedDirent::parse(&buf, off) {
                    if entry.ino != 0 && !entry.name.is_empty() {
                        let name = String::from_utf8_lossy(entry.name).to_string();
                        if name != "." && name != ".." {
                            entries.push(DirEntry { ino: entry.ino, file_type: entry.file_type, name });
                        }
                    }
                }
                off += rec_len;
            }
        }
        Ok(entries)
    }

    /// Insert a new `(name -> ino)` directory entry into `dir_raw`'s data,
    /// splitting an existing entry's slack space (real ext2's own
    /// approach) if one is big enough, or reusing a deleted (`inode == 0`)
    /// slot, or — only if nothing fits — allocating and appending a whole
    /// new directory block.
    pub fn add_dir_entry(&self, dir_ino: u32, dir_raw: &mut RawInode, name: &str, ino: u32, file_type: u8) -> Result<(), Ext2Error> {
        let bs = self.sb.block_size as usize;
        let needed = dirent_len(name.len());
        let num_blocks = ((dir_raw.size() as usize) + bs - 1) / bs;

        for block_index in 0..num_blocks as u32 {
            let Some(block_num) = self.block_for_index(dir_raw, block_index)? else { continue };
            let mut buf = self.block_vec(block_num)?;
            let mut off = 0usize;
            while off + 8 <= buf.len() {
                let entry_ino = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
                let rec_len = u16::from_le_bytes(buf[off + 4..off + 6].try_into().unwrap()) as usize;
                if rec_len < 8 {
                    return Err(Ext2Error::Io); // corrupt directory
                }
                let name_len = buf[off + 6] as usize;
                let used_len = if entry_ino == 0 { 0 } else { dirent_len(name_len) };
                let slack = rec_len - used_len;

                if slack >= needed {
                    if entry_ino != 0 {
                        // Split: shrink the existing entry to its real
                        // length, place the new one in the freed tail.
                        buf[off + 4..off + 6].copy_from_slice(&(used_len as u16).to_le_bytes());
                        let new_off = off + used_len;
                        write_dirent(&mut buf[new_off..new_off + slack], ino, slack as u16, name, file_type);
                    } else {
                        // Reuse a deleted slot in place, keeping its rec_len.
                        write_dirent(&mut buf[off..off + rec_len], ino, rec_len as u16, name, file_type);
                    }
                    self.write_block(block_num, &buf)?;
                    return Ok(());
                }
                off += rec_len;
            }
        }

        // No room anywhere — grow the directory by one block.
        let new_block_index = num_blocks as u32;
        let new_block = self.block_for_index_alloc(dir_raw, new_block_index)?;
        let mut buf = alloc::vec![0u8; bs];
        write_dirent(&mut buf[..], ino, bs as u16, name, file_type);
        self.write_block(new_block, &buf)?;
        dir_raw.set_size((new_block_index as u64 + 1) * bs as u64);
        self.write_inode(dir_ino, dir_raw)?;
        Ok(())
    }

    /// Remove the directory entry named `name` from `dir_raw`'s data.
    /// Merges its `rec_len` into the previous entry in the same block
    /// (real ext2's approach), or — if it's the first entry in the block —
    /// just zeroes its inode field, leaving a reusable deleted slot.
    /// Returns the removed entry's inode number and raw on-disk file-type
    /// byte.
    pub fn remove_dir_entry(&self, dir_raw: &RawInode, name: &str) -> Result<(u32, u8), Ext2Error> {
        let bs = self.sb.block_size as usize;
        let num_blocks = ((dir_raw.size() as usize) + bs - 1) / bs;

        for block_index in 0..num_blocks as u32 {
            let Some(block_num) = self.block_for_index(dir_raw, block_index)? else { continue };
            let mut buf = self.block_vec(block_num)?;
            let mut off = 0usize;
            let mut prev_off: Option<usize> = None;
            while off + 8 <= buf.len() {
                let entry_ino = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
                let rec_len = u16::from_le_bytes(buf[off + 4..off + 6].try_into().unwrap()) as usize;
                if rec_len < 8 {
                    break;
                }
                let name_len = buf[off + 6] as usize;
                let file_type = buf[off + 7];
                if entry_ino != 0 && name_len == name.len() && off + 8 + name_len <= buf.len()
                    && &buf[off + 8..off + 8 + name_len] == name.as_bytes()
                {
                    if let Some(p) = prev_off {
                        let p_rec_len = u16::from_le_bytes(buf[p + 4..p + 6].try_into().unwrap()) as usize;
                        buf[p + 4..p + 6].copy_from_slice(&((p_rec_len + rec_len) as u16).to_le_bytes());
                    } else {
                        buf[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
                    }
                    self.write_block(block_num, &buf)?;
                    return Ok((entry_ino, file_type));
                }
                prev_off = Some(off);
                off += rec_len;
            }
        }
        Err(Ext2Error::NotFound)
    }

    /// Rewrite a directory's `".."` entry to point at `new_parent_ino` —
    /// used when moving (rename) a subdirectory to a different parent.
    /// `".."` is always in the directory's first data block (it's written
    /// there by `mkdir` and this driver never reorders entries).
    pub fn set_dotdot(&self, dir_raw: &RawInode, new_parent_ino: u32) -> Result<(), Ext2Error> {
        let Some(block_num) = self.block_for_index(dir_raw, 0)? else { return Err(Ext2Error::Io) };
        let mut buf = self.block_vec(block_num)?;
        let mut off = 0usize;
        while off + 8 <= buf.len() {
            let rec_len = u16::from_le_bytes(buf[off + 4..off + 6].try_into().unwrap()) as usize;
            if rec_len < 8 {
                break;
            }
            let name_len = buf[off + 6] as usize;
            if name_len == 2 && off + 8 + 2 <= buf.len() && &buf[off + 8..off + 10] == b".." {
                buf[off..off + 4].copy_from_slice(&new_parent_ino.to_le_bytes());
                self.write_block(block_num, &buf)?;
                return Ok(());
            }
            off += rec_len;
        }
        Err(Ext2Error::Io)
    }

    // ── Symlinks ─────────────────────────────────────────────────────────

    /// Read a symlink inode's target string. Real ext2 has two on-disk
    /// representations, and this driver reads both: "fast" — target under
    /// 60 bytes, stored directly in the inode's `i_block` bytes, no data
    /// block ever allocated — and "slow" — target stored as ordinary file
    /// content, same as a regular file. `size < 60` (not, say,
    /// `i_block(0) == 0`) is the only reliable way to tell them apart
    /// *while reading*: the fast representation's inline storage bytes
    /// physically alias `i_block`'s own byte range (that's the whole
    /// space-saving trick — see `write_symlink_target` below), so a short
    /// target whose own bytes happen to decode to a nonzero `i_block(0)`
    /// would otherwise be misread as a slow symlink and its text bytes
    /// reinterpreted as real block pointers. `size < 60` has no such
    /// ambiguity: a target that size can only ever have been written as
    /// "fast" (60 bytes is the hard physical limit of the inline area, on
    /// any ext2 image, not just this driver's own writes), so this matches
    /// both this driver's own symlinks and a real `mke2fs`/host-authored
    /// image's.
    pub fn read_symlink_target(&self, raw: &RawInode) -> Result<String, Ext2Error> {
        let size = raw.size() as usize;
        if raw.is_fast_symlink() {
            let bytes = &raw.buf[40..40 + size];
            return Ok(String::from_utf8_lossy(bytes).to_string());
        }
        let mut buf = alloc::vec![0u8; size];
        self.read_file_range(raw, 0, &mut buf)?;
        Ok(String::from_utf8_lossy(&buf).to_string())
    }

    /// Write `target` as `ino`'s symlink content, choosing whichever of
    /// ext2's two on-disk representations fits (see `read_symlink_target`'s
    /// doc comment above): "fast" (inline in `i_block`'s own bytes, no data
    /// block ever allocated) when `target.len() < 60`, "slow" (ordinary
    /// file content) otherwise. Caller (the kernel adapter's `symlink()`)
    /// is responsible for everything around this: allocating `ino` and
    /// setting its mode/links count before calling this, and cleaning up
    /// (`free_all_blocks`/`free_inode`) if this returns `Err` — this
    /// function only ever writes forward, never rolls back its own partial
    /// work, same as `write_file_range` it delegates to for the slow case.
    pub fn write_symlink_target(&self, raw: &mut RawInode, ino: u32, target: &str) -> Result<(), Ext2Error> {
        if target.len() < 60 {
            // Fast symlink: target lives directly in the i_block bytes, no
            // data block allocated.
            raw.buf[40..40 + target.len()].copy_from_slice(target.as_bytes());
            raw.set_size(target.len() as u64);
            self.write_inode(ino, raw)
        } else {
            // Slow symlink: persist mode/links first (same "write content
            // before linking" ordering as `create`), then grow it exactly
            // like a regular file's content.
            self.write_inode(ino, raw)?;
            self.write_file_range(ino, raw, 0, target.as_bytes())?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dirent::ParsedDirent;
    use crate::superblock::ROOT_INO;
    use crate::test_support::{minimal_image, mount};

    /// `add_dir_entry`/`remove_dir_entry` below all take a valid,
    /// already-allocated inode number purely to read/write through the
    /// inode table — `ROOT_INO` (2) is the one `minimal_image()` marks
    /// used, same convention `volume.rs`'s own migration-step-3 tests
    /// established. The `RawInode` passed alongside it is a fresh
    /// in-memory directory built by each test, not read back from disk
    /// first (`minimal_image()` deliberately writes no real directory data
    /// for root — see `test_support`'s doc comment).
    fn new_dir_raw() -> RawInode {
        let mut raw = RawInode::zeroed(128);
        raw.set_i_mode(0x4000 | 0o755);
        raw
    }

    #[test]
    fn add_dir_entry_then_read_dir_entries_finds_it() {
        let core = mount(minimal_image());
        let mut dir_raw = new_dir_raw();
        core.add_dir_entry(ROOT_INO, &mut dir_raw, "foo", 5, 1).expect("add");

        let entries = core.read_dir_entries(&dir_raw).expect("read");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].ino, 5);
        assert_eq!(entries[0].file_type, 1);
        assert_eq!(entries[0].name, "foo");
    }

    #[test]
    fn read_dir_entries_never_reports_dot_or_dotdot() {
        let core = mount(minimal_image());
        let mut dir_raw = new_dir_raw();
        core.add_dir_entry(ROOT_INO, &mut dir_raw, ".", ROOT_INO, 2).expect("add .");
        core.add_dir_entry(ROOT_INO, &mut dir_raw, "..", ROOT_INO, 2).expect("add ..");
        core.add_dir_entry(ROOT_INO, &mut dir_raw, "real", 9, 1).expect("add real");

        let entries = core.read_dir_entries(&dir_raw).expect("read");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "real");
    }

    #[test]
    fn add_dir_entry_grows_directory_by_one_block_when_full() {
        let core = mount(minimal_image());
        let mut dir_raw = new_dir_raw();
        assert_eq!(dir_raw.size(), 0);
        core.add_dir_entry(ROOT_INO, &mut dir_raw, "first", 10, 1).expect("add");
        // First entry claims the whole freshly-allocated block.
        assert_eq!(dir_raw.size(), core.sb.block_size as u64);
        assert_ne!(dir_raw.i_block(0), 0);
    }

    #[test]
    fn add_dir_entry_splits_slack_for_a_second_entry_without_growing() {
        let core = mount(minimal_image());
        let mut dir_raw = new_dir_raw();
        core.add_dir_entry(ROOT_INO, &mut dir_raw, "first", 10, 1).expect("add 1");
        let size_after_first = dir_raw.size();

        core.add_dir_entry(ROOT_INO, &mut dir_raw, "second", 11, 1).expect("add 2");
        assert_eq!(dir_raw.size(), size_after_first, "second entry must reuse slack, not grow the directory");

        let entries = core.read_dir_entries(&dir_raw).expect("read");
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|e| e.name == "first" && e.ino == 10));
        assert!(entries.iter().any(|e| e.name == "second" && e.ino == 11));
    }

    #[test]
    fn remove_dir_entry_returns_ino_and_file_type_and_it_is_gone() {
        let core = mount(minimal_image());
        let mut dir_raw = new_dir_raw();
        core.add_dir_entry(ROOT_INO, &mut dir_raw, "gone", 7, 2).expect("add");

        let (ino, file_type) = core.remove_dir_entry(&dir_raw, "gone").expect("remove");
        assert_eq!(ino, 7);
        assert_eq!(file_type, 2);

        let entries = core.read_dir_entries(&dir_raw).expect("read");
        assert!(entries.is_empty());
    }

    #[test]
    fn remove_dir_entry_missing_name_is_not_found() {
        let core = mount(minimal_image());
        let dir_raw = new_dir_raw(); // never had any entry added, size 0
        assert_eq!(core.remove_dir_entry(&dir_raw, "nope"), Err(Ext2Error::NotFound));
    }

    #[test]
    fn removed_slot_is_reused_by_a_later_add_without_growing() {
        let core = mount(minimal_image());
        let mut dir_raw = new_dir_raw();
        core.add_dir_entry(ROOT_INO, &mut dir_raw, "first", 10, 1).expect("add 1");
        core.add_dir_entry(ROOT_INO, &mut dir_raw, "second", 11, 1).expect("add 2");
        let size_before = dir_raw.size();

        core.remove_dir_entry(&dir_raw, "first").expect("remove first");
        core.add_dir_entry(ROOT_INO, &mut dir_raw, "third", 12, 1).expect("add 3");

        assert_eq!(dir_raw.size(), size_before, "reusing a deleted slot must not grow the directory");
        let entries = core.read_dir_entries(&dir_raw).expect("read");
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|e| e.name == "second"));
        assert!(entries.iter().any(|e| e.name == "third" && e.ino == 12));
    }

    #[test]
    fn set_dotdot_rewrites_only_the_dotdot_entry() {
        let core = mount(minimal_image());
        let mut dir_raw = new_dir_raw();
        core.add_dir_entry(ROOT_INO, &mut dir_raw, ".", 99, 2).expect("add .");
        core.add_dir_entry(ROOT_INO, &mut dir_raw, "..", 7, 2).expect("add ..");

        core.set_dotdot(&dir_raw, 42).expect("set_dotdot");

        let block_num = core.block_for_index(&dir_raw, 0).unwrap().expect("block 0");
        let buf = core.block_vec(block_num).expect("read block");
        let dot = ParsedDirent::parse(&buf, 0).expect("dot entry");
        assert_eq!(dot.name, b".");
        assert_eq!(dot.ino, 99, "unrelated '.' entry must survive untouched");

        let dotdot = ParsedDirent::parse(&buf, dot.rec_len as usize).expect("dotdot entry");
        assert_eq!(dotdot.name, b"..");
        assert_eq!(dotdot.ino, 42, "'..' must now point at the new parent");
    }

    #[test]
    fn set_dotdot_on_directory_with_no_dotdot_entry_is_io_error() {
        let core = mount(minimal_image());
        let mut dir_raw = new_dir_raw();
        core.add_dir_entry(ROOT_INO, &mut dir_raw, "onlyentry", 5, 1).expect("add");
        assert_eq!(core.set_dotdot(&dir_raw, 42), Err(Ext2Error::Io));
    }

    #[test]
    fn write_symlink_target_fast_short_target_stays_inline() {
        let core = mount(minimal_image());
        let free_blocks_before = core.read_bgd(0).unwrap().free_blocks;

        let mut raw = RawInode::zeroed(128);
        raw.set_i_mode(0xA000 | 0o777);
        core.write_symlink_target(&mut raw, ROOT_INO, "/bin/sh").expect("write");

        assert!(raw.is_fast_symlink());
        // `i_block(0)`'s bytes physically alias the inline target text (the
        // whole "fast" space-saving trick — see `read_symlink_target`'s doc
        // comment), so it's expected to read back as whatever garbage
        // "/bin/sh"'s first 4 bytes decode to, NOT zero. What actually
        // proves no data block was allocated is that the free-block count
        // never moved.
        let free_blocks_after = core.read_bgd(0).unwrap().free_blocks;
        assert_eq!(free_blocks_after, free_blocks_before, "fast symlink must never allocate a data block");
        assert_eq!(core.read_symlink_target(&raw).expect("read"), "/bin/sh");
    }

    #[test]
    fn write_symlink_target_slow_long_target_uses_a_data_block() {
        let core = mount(minimal_image());
        let mut raw = RawInode::zeroed(128);
        raw.set_i_mode(0xA000 | 0o777);
        let target: String = "a".repeat(100); // >= 60 bytes -> slow representation

        core.write_symlink_target(&mut raw, ROOT_INO, &target).expect("write");

        assert!(!raw.is_fast_symlink());
        assert_ne!(raw.i_block(0), 0, "slow symlink must allocate a real data block");
        assert_eq!(core.read_symlink_target(&raw).expect("read"), target);
    }

    #[test]
    fn write_symlink_target_boundary_59_bytes_is_fast_60_bytes_is_slow() {
        let core = mount(minimal_image());

        let mut fast = RawInode::zeroed(128);
        fast.set_i_mode(0xA000 | 0o777);
        let t59 = "b".repeat(59);
        core.write_symlink_target(&mut fast, ROOT_INO, &t59).expect("write 59");
        assert!(fast.is_fast_symlink());

        let mut slow = RawInode::zeroed(128);
        slow.set_i_mode(0xA000 | 0o777);
        let t60 = "c".repeat(60);
        core.write_symlink_target(&mut slow, ROOT_INO, &t60).expect("write 60");
        assert!(!slow.is_fast_symlink());
    }
}
