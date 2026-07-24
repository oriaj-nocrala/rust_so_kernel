// ext2/src/test_support.rs
//
// Shared `#[cfg(test)]` fixture for this crate's own unit tests: a minimal,
// self-consistent rev-0 ext2 image on a `hal::block::MemDisk` (one block
// group, 1024-byte blocks, root inode (ino 2) already allocated in the
// bitmap — but with no actual directory data written for it, see below).
//
// Originally lived as a private helper inside `volume.rs`'s own `mod
// tests`; pulled out here (migration step 4,
// `docs/fs/ext2-extraction-plan.md`) so `dir.rs`'s new directory-operation
// tests can reuse the exact same fixture instead of hand-rolling a second,
// possibly-drifting copy of the same on-disk layout. No behavior change to
// the fixture itself.
//
// Note this is deliberately NOT the same fixture as
// [`crate::testimg::build_minimal_image`] — that one also writes a real
// "."/".." directory data block for root. This one doesn't: every test that
// needs a directory to operate on builds its own `RawInode` in memory (via
// `RawInode::zeroed`) and passes it directly to whatever `Ext2Core` method
// is under test, using `ROOT_INO` purely as "a valid, already-allocated
// inode number to read/write through" — same convention `volume.rs`'s own
// migration-step-3 tests already established. `dir.rs`/`volume.rs` keep
// using this smaller fixture rather than `testimg::build_minimal_image`
// because several of their assertions are pinned to this fixture's exact
// geometry (e.g. `blocks_in_group(0) == 63`, `inodes_in_group(0) == 32`,
// both derived from this function's 64-block/32-inode size) — swapping in
// the much larger `testimg` fixture would silently break those numbers.
//
// The other former resident of this file, `build_image_with_orphans` (plus
// its `ORPHAN_*`/`PHANTOM_*` constants), moved to `crate::testimg`
// (migration step 6) — it was a byte-for-byte duplicate of
// `kernel::fs::ext2`'s own copy of the same fixture, kept in sync by hand
// across the crate boundary for no real benefit once both sides could
// import a single shared definition instead.
#![cfg(test)]

use alloc::boxed::Box;
use hal::block::MemDisk;

use crate::volume::Ext2Core;

pub(crate) fn minimal_image() -> MemDisk {
    const BLOCK_SIZE: u32 = 1024;
    const TOTAL_BLOCKS: u32 = 64;
    const INODES_COUNT: u32 = 32;
    const INODE_SIZE: u32 = 128;
    const FIRST_DATA_BLOCK: u32 = 1;
    const BGDT_BLOCK: u32 = FIRST_DATA_BLOCK + 1;
    const BLOCK_BITMAP_BLOCK: u32 = 3;
    const INODE_BITMAP_BLOCK: u32 = 4;
    const INODE_TABLE_START: u32 = 5;

    let inodes_per_block = BLOCK_SIZE / INODE_SIZE; // 8
    let inode_table_blocks = (INODES_COUNT + inodes_per_block - 1) / inodes_per_block; // 4
    let root_data_block = INODE_TABLE_START + inode_table_blocks; // 9

    let mut img = alloc::vec![0u8; (TOTAL_BLOCKS * BLOCK_SIZE) as usize];
    let put_block = |img: &mut alloc::vec::Vec<u8>, block_num: u32, data: &[u8]| {
        let off = (block_num * BLOCK_SIZE) as usize;
        img[off..off + data.len()].copy_from_slice(data);
    };

    let used_block_bits = root_data_block - FIRST_DATA_BLOCK + 1;
    let blocks_in_group0 = TOTAL_BLOCKS - FIRST_DATA_BLOCK;
    let free_blocks = blocks_in_group0 - used_block_bits;
    let free_inodes = INODES_COUNT - 1;

    // Superblock.
    let mut sb = alloc::vec![0u8; BLOCK_SIZE as usize];
    sb[0..4].copy_from_slice(&INODES_COUNT.to_le_bytes());
    sb[4..8].copy_from_slice(&TOTAL_BLOCKS.to_le_bytes());
    sb[12..16].copy_from_slice(&free_blocks.to_le_bytes());
    sb[16..20].copy_from_slice(&free_inodes.to_le_bytes());
    sb[20..24].copy_from_slice(&FIRST_DATA_BLOCK.to_le_bytes());
    sb[24..28].copy_from_slice(&0u32.to_le_bytes());
    sb[32..36].copy_from_slice(&TOTAL_BLOCKS.to_le_bytes()); // one group
    sb[40..44].copy_from_slice(&INODES_COUNT.to_le_bytes());
    sb[56..58].copy_from_slice(&crate::superblock::EXT2_MAGIC.to_le_bytes());
    put_block(&mut img, FIRST_DATA_BLOCK, &sb);

    // Block group descriptor.
    let mut bgd_buf = alloc::vec![0u8; 32];
    bgd_buf[0..4].copy_from_slice(&BLOCK_BITMAP_BLOCK.to_le_bytes());
    bgd_buf[4..8].copy_from_slice(&INODE_BITMAP_BLOCK.to_le_bytes());
    bgd_buf[8..12].copy_from_slice(&INODE_TABLE_START.to_le_bytes());
    bgd_buf[12..14].copy_from_slice(&(free_blocks as u16).to_le_bytes());
    bgd_buf[14..16].copy_from_slice(&(free_inodes as u16).to_le_bytes());
    let mut bgdt_block_buf = alloc::vec![0u8; BLOCK_SIZE as usize];
    bgdt_block_buf[0..32].copy_from_slice(&bgd_buf);
    put_block(&mut img, BGDT_BLOCK, &bgdt_block_buf);

    // Block bitmap: metadata footprint used.
    let mut block_bitmap = alloc::vec![0u8; BLOCK_SIZE as usize];
    for bit in 0..used_block_bits {
        block_bitmap[(bit / 8) as usize] |= 1u8 << (bit % 8);
    }
    put_block(&mut img, BLOCK_BITMAP_BLOCK, &block_bitmap);

    // Inode bitmap: only root (ino 2) used.
    let mut inode_bitmap = alloc::vec![0u8; BLOCK_SIZE as usize];
    inode_bitmap[0] |= 1u8 << (crate::superblock::ROOT_INO - 1);
    put_block(&mut img, INODE_BITMAP_BLOCK, &inode_bitmap);

    MemDisk::from_vec(img)
}

pub(crate) fn mount(disk: MemDisk) -> Ext2Core {
    Ext2Core::mount(Box::new(disk)).expect("mount")
}
