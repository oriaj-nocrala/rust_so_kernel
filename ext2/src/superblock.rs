// ext2/src/superblock.rs
//
// The 1024-byte ext2 superblock: on-disk constants + the subset of fields
// this driver reads, plus pure parsing (`Superblock::parse`). Moved
// verbatim (same field semantics, same validation, same error conditions)
// out of what used to be inline in `kernel::fs::ext2::Ext2Fs::mount()` —
// migration step 1, no behavior change.

use crate::error::Ext2Error;

/// `s_magic` — every real ext2/3/4 superblock has this at byte offset 56,
/// regardless of revision or feature bits.
pub const EXT2_MAGIC: u16 = 0xEF53;

/// The root directory's inode number. Fixed by the ext2 on-disk format
/// itself, not configurable — inode 1 is reserved (bad-blocks), inode 2 is
/// always root.
pub const ROOT_INO: u32 = 2;

/// `s_feature_incompat` bit for "directory entries carry a file-type byte"
/// (`ext2_dir_entry_2`'s `file_type` field, vs. the older `ext2_dir_entry`
/// which doesn't have one). The only incompat feature this driver
/// understands — anything else (extents, a journal, ...) means `i_block`
/// means something this driver doesn't know how to read, so mounting
/// refuses rather than guess.
pub const FEATURE_INCOMPAT_FILETYPE: u32 = 0x0002;

/// Parsed, filesystem-wide geometry read once at mount time from the
/// superblock — everything block/inode arithmetic elsewhere in this crate
/// needs to turn an inode number or block index into a real on-disk
/// location. Fields here never change after mount; the on-disk
/// superblock's own free-block/free-inode *counters* do change (see
/// `volume::Ext2Core::adjust_sb_counts`), but those are re-read fresh from
/// disk whenever needed rather than cached in this struct, exactly like
/// the code this replaces did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Superblock {
    pub block_size: u32,
    pub inodes_count: u32,
    pub blocks_count: u32,
    pub inodes_per_group: u32,
    pub blocks_per_group: u32,
    pub first_data_block: u32,
    pub inode_size: u16,
    /// Block holding the block-group descriptor table — always one block
    /// past the superblock's own block (`first_data_block + 1`).
    pub bgdt_block: u32,
    pub num_groups: u32,
    /// First non-reserved inode number (`s_first_ino`, rev>=1 only; fixed
    /// at 11 for rev 0) — inodes below this (root's own ino=2 among them)
    /// are always "in use" by convention, even though nothing walks a
    /// directory entry to most of them. Used by the mount-time orphan scan
    /// (`kernel::fs::ext2::Ext2Fs::reclaim_orphans`, not yet moved into
    /// this crate) so it doesn't mistake a reserved inode for an orphan.
    pub first_ino: u32,
}

impl Superblock {
    /// Parse the 1024-byte superblock. `raw` must be at least 1024 bytes
    /// (the fixed on-disk superblock size, always at byte offset 1024
    /// regardless of block size — callers read it by fixed sector before
    /// the block size is even known).
    ///
    /// Validates the magic number and refuses any `s_feature_incompat` bit
    /// beyond FILETYPE — see `FEATURE_INCOMPAT_FILETYPE`'s doc comment.
    pub fn parse(raw: &[u8]) -> Result<Self, Ext2Error> {
        if raw.len() < 1024 {
            return Err(Ext2Error::Io);
        }

        let magic = u16::from_le_bytes([raw[56], raw[57]]);
        if magic != EXT2_MAGIC {
            return Err(Ext2Error::BadMagic);
        }

        let log_block_size = u32::from_le_bytes(raw[24..28].try_into().unwrap());
        let block_size = 1024u32 << log_block_size;
        let blocks_per_group = u32::from_le_bytes(raw[32..36].try_into().unwrap());
        let inodes_per_group = u32::from_le_bytes(raw[40..44].try_into().unwrap());
        let inodes_count = u32::from_le_bytes(raw[0..4].try_into().unwrap());
        let blocks_count = u32::from_le_bytes(raw[4..8].try_into().unwrap());
        let first_data_block = u32::from_le_bytes(raw[20..24].try_into().unwrap());
        let rev_level = u32::from_le_bytes(raw[76..80].try_into().unwrap());

        let inode_size = if rev_level == 0 {
            128
        } else {
            u16::from_le_bytes(raw[88..90].try_into().unwrap())
        };
        let feature_incompat = if rev_level == 0 {
            0
        } else {
            u32::from_le_bytes(raw[96..100].try_into().unwrap())
        };
        let first_ino = if rev_level == 0 {
            11
        } else {
            u32::from_le_bytes(raw[84..88].try_into().unwrap())
        };

        if feature_incompat & !FEATURE_INCOMPAT_FILETYPE != 0 {
            return Err(Ext2Error::UnsupportedFeature);
        }

        let num_groups = (blocks_count + blocks_per_group - 1) / blocks_per_group;
        let bgdt_block = first_data_block + 1;

        Ok(Self {
            block_size,
            inodes_count,
            blocks_count,
            inodes_per_group: inodes_per_group.max(1),
            blocks_per_group: blocks_per_group.max(1),
            first_data_block,
            inode_size: if inode_size == 0 { 128 } else { inode_size },
            bgdt_block,
            num_groups: num_groups.max(1),
            first_ino: if first_ino == 0 { 11 } else { first_ino },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal valid rev-0 superblock byte image, matching the
    /// exact field layout this crate/its kernel predecessor reads. Mirrors
    /// `kernel::fs::ext2::build_minimal_image`'s superblock section.
    fn minimal_sb_bytes() -> [u8; 1024] {
        let mut sb = [0u8; 1024];
        let inodes_count: u32 = 128;
        let blocks_count: u32 = 256;
        let free_blocks: u32 = 200;
        let free_inodes: u32 = 100;
        let first_data_block: u32 = 1;
        let blocks_per_group: u32 = 256;
        let inodes_per_group: u32 = 128;
        sb[0..4].copy_from_slice(&inodes_count.to_le_bytes());
        sb[4..8].copy_from_slice(&blocks_count.to_le_bytes());
        sb[12..16].copy_from_slice(&free_blocks.to_le_bytes());
        sb[16..20].copy_from_slice(&free_inodes.to_le_bytes());
        sb[20..24].copy_from_slice(&first_data_block.to_le_bytes());
        sb[24..28].copy_from_slice(&0u32.to_le_bytes()); // log_block_size=0 -> 1024
        sb[32..36].copy_from_slice(&blocks_per_group.to_le_bytes());
        sb[40..44].copy_from_slice(&inodes_per_group.to_le_bytes());
        sb[56..58].copy_from_slice(&EXT2_MAGIC.to_le_bytes());
        // s_rev_level (offset 76) left at 0 -> rev 0 defaults apply.
        sb
    }

    #[test]
    fn parses_minimal_rev0_superblock() {
        let sb = Superblock::parse(&minimal_sb_bytes()).expect("parse");
        assert_eq!(sb.block_size, 1024);
        assert_eq!(sb.inodes_count, 128);
        assert_eq!(sb.blocks_count, 256);
        assert_eq!(sb.blocks_per_group, 256);
        assert_eq!(sb.inodes_per_group, 128);
        assert_eq!(sb.first_data_block, 1);
        assert_eq!(sb.inode_size, 128); // rev 0 fixed default
        assert_eq!(sb.first_ino, 11); // rev 0 fixed default
        assert_eq!(sb.bgdt_block, 2); // first_data_block + 1
        assert_eq!(sb.num_groups, 1);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut raw = minimal_sb_bytes();
        raw[56] = 0x00;
        raw[57] = 0x00;
        assert_eq!(Superblock::parse(&raw), Err(Ext2Error::BadMagic));
    }

    #[test]
    fn rejects_short_buffer() {
        let raw = [0u8; 100];
        assert_eq!(Superblock::parse(&raw), Err(Ext2Error::Io));
    }

    #[test]
    fn rejects_unsupported_incompat_feature() {
        let mut raw = minimal_sb_bytes();
        // Bump to rev 1 so feature_incompat is actually read, and set a
        // bit beyond FILETYPE (e.g. EXTENTS = 0x0040, an ext4-only field).
        raw[76..80].copy_from_slice(&1u32.to_le_bytes());
        raw[88..90].copy_from_slice(&128u16.to_le_bytes()); // inode_size
        raw[84..88].copy_from_slice(&11u32.to_le_bytes()); // first_ino
        raw[96..100].copy_from_slice(&0x0040u32.to_le_bytes());
        assert_eq!(Superblock::parse(&raw), Err(Ext2Error::UnsupportedFeature));
    }

    #[test]
    fn accepts_filetype_incompat_feature() {
        let mut raw = minimal_sb_bytes();
        raw[76..80].copy_from_slice(&1u32.to_le_bytes());
        raw[88..90].copy_from_slice(&128u16.to_le_bytes());
        raw[84..88].copy_from_slice(&11u32.to_le_bytes());
        raw[96..100].copy_from_slice(&FEATURE_INCOMPAT_FILETYPE.to_le_bytes());
        let sb = Superblock::parse(&raw).expect("parse");
        assert_eq!(sb.first_ino, 11);
    }

    #[test]
    fn rev1_reads_custom_inode_size_and_first_ino() {
        let mut raw = minimal_sb_bytes();
        raw[76..80].copy_from_slice(&1u32.to_le_bytes());
        raw[88..90].copy_from_slice(&256u16.to_le_bytes());
        raw[84..88].copy_from_slice(&64u32.to_le_bytes());
        let sb = Superblock::parse(&raw).expect("parse");
        assert_eq!(sb.inode_size, 256);
        assert_eq!(sb.first_ino, 64);
    }

    #[test]
    fn num_groups_rounds_up() {
        let mut raw = minimal_sb_bytes();
        // 256 blocks, but only 100 per group -> 3 groups (ceil(256/100)).
        raw[32..36].copy_from_slice(&100u32.to_le_bytes());
        let sb = Superblock::parse(&raw).expect("parse");
        assert_eq!(sb.num_groups, 3);
    }
}
