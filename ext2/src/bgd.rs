// ext2/src/bgd.rs
//
// Block group descriptor: the subset of fields this driver reads/writes,
// plus pure parsing and the pure block-group-descriptor-table location
// arithmetic. Moved verbatim out of `kernel::fs::ext2`'s `BgdRaw` struct +
// `bgd_location` — migration step 1, no behavior change.

use crate::superblock::Superblock;

/// The subset of a 32-byte on-disk block group descriptor this driver
/// reads/writes: `bg_block_bitmap`, `bg_inode_bitmap`, `bg_inode_table`,
/// `bg_free_blocks_count`, `bg_free_inodes_count`. `bg_used_dirs_count`
/// (offset +16) is intentionally not a field here — it's only ever
/// adjusted, never read back, and `volume::Ext2Core::adjust_bgd_counts`
/// patches it directly by byte offset instead, matching the original code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockGroupDesc {
    pub block_bitmap: u32,
    pub inode_bitmap: u32,
    pub inode_table: u32,
    pub free_blocks: u16,
    pub free_inodes: u16,
}

impl BlockGroupDesc {
    /// Parse one descriptor out of `buf`, starting at `buf[0]` — callers
    /// slice to the right offset first (see `bgd_location`).
    pub fn parse(buf: &[u8]) -> Self {
        Self {
            block_bitmap: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            inode_bitmap: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            inode_table: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            free_blocks: u16::from_le_bytes(buf[12..14].try_into().unwrap()),
            free_inodes: u16::from_le_bytes(buf[14..16].try_into().unwrap()),
        }
    }
}

/// Locate the block + byte offset of block group `group`'s descriptor
/// inside the block-group descriptor table (`sb.bgdt_block`, one block
/// past the superblock) — 32 bytes per descriptor, however many fit in one
/// filesystem block.
pub fn bgd_location(sb: &Superblock, group: u32) -> (u32, usize) {
    let bgd_per_block = sb.block_size / 32;
    let bgd_block = sb.bgdt_block + group / bgd_per_block;
    let bgd_offset = ((group % bgd_per_block) * 32) as usize;
    (bgd_block, bgd_offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sb_1024_block(bgdt_block: u32) -> Superblock {
        Superblock {
            block_size: 1024,
            inodes_count: 128,
            blocks_count: 256,
            inodes_per_group: 128,
            blocks_per_group: 256,
            first_data_block: 1,
            inode_size: 128,
            bgdt_block,
            num_groups: 1,
            first_ino: 11,
        }
    }

    #[test]
    fn parses_known_bytes() {
        let mut buf = [0u8; 32];
        buf[0..4].copy_from_slice(&3u32.to_le_bytes());
        buf[4..8].copy_from_slice(&4u32.to_le_bytes());
        buf[8..12].copy_from_slice(&5u32.to_le_bytes());
        buf[12..14].copy_from_slice(&200u16.to_le_bytes());
        buf[14..16].copy_from_slice(&100u16.to_le_bytes());
        let bgd = BlockGroupDesc::parse(&buf);
        assert_eq!(bgd, BlockGroupDesc {
            block_bitmap: 3,
            inode_bitmap: 4,
            inode_table: 5,
            free_blocks: 200,
            free_inodes: 100,
        });
    }

    #[test]
    fn parses_at_nonzero_offset_within_a_larger_buffer() {
        // Simulate slicing a whole block down to the group's own 32 bytes,
        // preceded by another group's descriptor.
        let mut block = [0xAAu8; 64];
        block[32..36].copy_from_slice(&99u32.to_le_bytes());
        let bgd = BlockGroupDesc::parse(&block[32..]);
        assert_eq!(bgd.block_bitmap, 99);
    }

    #[test]
    fn bgd_location_first_group_is_bgdt_block_offset_zero() {
        let sb = sb_1024_block(2);
        assert_eq!(bgd_location(&sb, 0), (2, 0));
    }

    #[test]
    fn bgd_location_within_first_block() {
        // 1024 / 32 = 32 descriptors per block.
        let sb = sb_1024_block(2);
        assert_eq!(bgd_location(&sb, 1), (2, 32));
        assert_eq!(bgd_location(&sb, 31), (2, 31 * 32));
    }

    #[test]
    fn bgd_location_spills_into_next_block() {
        let sb = sb_1024_block(2);
        // Group 32 is the first entry of the second BGDT block.
        assert_eq!(bgd_location(&sb, 32), (3, 0));
        assert_eq!(bgd_location(&sb, 33), (3, 32));
    }
}
