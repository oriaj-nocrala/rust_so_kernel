// ext2/src/testimg.rs
//
// Hand-built, self-contained ext2 disk images (`Vec<u8>`) used only by
// tests — this crate's own host tests (`repair::tests`) and the kernel's
// QEMU integration test (`kernel/src/hw_tests.rs`). Deliberately NOT
// `#[cfg(test)]`: `hw_tests.rs`'s `#[test_case]`s are collected into a
// real kernel *binary* built via `cargo build --target x86_64-unknown-none
// --tests` (see CLAUDE.md's "QEMU integration tests" section — plain
// `cargo test` doesn't work on this target), so this crate needs these
// builders compiled into its ordinary (non-test) build for that binary to
// link against them at all. Pure `alloc`, no I/O, no host-only surface —
// builds fine under `-Z build-std` on the bare-metal target too.
//
// Consolidated here (migration step 6, `docs/fs/ext2-extraction-plan.md`)
// out of two former duplicate copies that both existed only to feed a
// test: `kernel::fs::ext2`'s own `#[cfg(test)]` copy (used by
// `hw_tests.rs`) and `ext2::test_support`'s `#[cfg(test)]` copy (used by
// this crate's own `repair::tests`). Both were byte-for-byte identical —
// each side's doc comment already called the other an accepted,
// "moved verbatim" duplicate. This module is now the single source both
// call sites import from.
//
// `test_support::minimal_image()` (a *different*, much smaller fixture —
// 64 blocks / 32 inodes, no real root directory data ever written) is
// deliberately NOT folded in here: `dir.rs`'s and `volume.rs`'s own unit
// tests assert exact geometry numbers derived from that fixture's specific
// size (e.g. `blocks_in_group(0) == 63`, `inodes_in_group(0) == 32`), so
// swapping it for this module's much larger `build_minimal_image()` would
// silently break those assertions for no real benefit — see
// `test_support`'s own doc comment for the full rationale on why the two
// fixtures serve genuinely different purposes (unit-level bitmap/indirect-
// addressing exercises vs. a full VFS-integration-test disk).

use alloc::vec::Vec;
use hal::block::SECTOR_SIZE;

use crate::dirent::{dirent_len, write_dirent};
use crate::inode::RawInode;
use crate::repair::{mark_bit, mark_bit_1based};
use crate::superblock::{EXT2_MAGIC, ROOT_INO};

/// `ext2_dir_entry_2`'s raw `file_type` byte for a directory (this crate's
/// on-disk convention — see `dirent.rs`'s module doc comment for the full
/// byte->kind table).
const DIR_FILE_TYPE: u8 = 2;

// ── A minimal, self-consistent, mountable ext2 image ────────────────────
//
// Backs the QEMU integration test in `kernel/src/hw_tests.rs`
// (`ext2_memdisk_roundtrip`). Building a real on-disk image from scratch is
// normally `mke2fs`'s job (the workspace root `build.rs` already shells out
// to it for `disk.img`) — this doesn't duplicate that to work around a
// missing tool. The point is a *self-contained* image with zero host-tool
// dependency, built at kernel **runtime**, inside the test itself, instead
// of an embedded build-time artifact. That keeps the QEMU test binary
// buildable on any machine (no `mke2fs`/`e2fsprogs` requirement beyond what
// the root build already needs for `disk.img`) and, more importantly, means
// the test never touches the real disk.img at all.
//
// The image is deliberately the smallest thing that satisfies every field
// `Ext2Core::mount()`/`read_bgd`/`alloc_block`/`alloc_inode`/
// `reconcile_free_counts`/`reclaim_orphans` actually read: revision 0 (so
// `inode_size`=128 and `first_ino`=11 are the fixed rev-0 defaults, and
// `s_feature_incompat` can stay all-zero — no FILETYPE feature needed for
// this driver's own read/write round trip), a single block group, and
// free-count fields computed to exactly match what's actually marked used
// — so `reconcile_free_counts` and `reclaim_orphans` both find nothing to
// repair on a fresh mount, exactly like a real freshly-`mke2fs`'d image
// would.
//
// Layout (1024-byte blocks):
//   block 0        — boot block (unused)
//   block 1        — superblock          (first_data_block)
//   block 2        — block group descriptor table (1 group fits in 1 block)
//   block 3        — block bitmap
//   block 4        — inode bitmap
//   blocks 5..=20  — inode table (128 inodes * 128 bytes = 16 blocks)
//   block 21       — root directory data ("." and ".." only)
//   blocks 22..255 — free data blocks (234 of them — plenty for a test that
//                    creates a handful of files/dirs)
pub fn build_minimal_image() -> Vec<u8> {
    const BLOCK_SIZE: u32 = 1024;
    const TOTAL_BLOCKS: u32 = 256; // 256 KiB image
    const INODES_COUNT: u32 = 128;
    const INODE_SIZE: u32 = 128; // rev 0, fixed
    const FIRST_DATA_BLOCK: u32 = 1; // always 1 when block_size == 1024
    const BGDT_BLOCK: u32 = FIRST_DATA_BLOCK + 1;
    const BLOCK_BITMAP_BLOCK: u32 = 3;
    const INODE_BITMAP_BLOCK: u32 = 4;
    const INODE_TABLE_START: u32 = 5;

    let inodes_per_block = BLOCK_SIZE / INODE_SIZE; // 8
    let inode_table_blocks = (INODES_COUNT + inodes_per_block - 1) / inodes_per_block; // 16
    let root_data_block = INODE_TABLE_START + inode_table_blocks; // 21

    let mut img = alloc::vec![0u8; (TOTAL_BLOCKS * BLOCK_SIZE) as usize];
    let put_block = |img: &mut Vec<u8>, block_num: u32, data: &[u8]| {
        let off = (block_num * BLOCK_SIZE) as usize;
        img[off..off + data.len()].copy_from_slice(data);
    };

    // Every block from FIRST_DATA_BLOCK..=root_data_block is metadata the
    // block bitmap must mark used; group 0 covers all of `blocks_count`
    // here (a single group), so this is also the group's whole "used"
    // footprint before any test writes happen.
    let used_block_bits = root_data_block - FIRST_DATA_BLOCK + 1; // 21
    let blocks_per_group = TOTAL_BLOCKS; // one group covers the whole image
    let blocks_in_group0 = TOTAL_BLOCKS - FIRST_DATA_BLOCK; // 255 (block 0 excluded)
    let free_blocks = blocks_in_group0 - used_block_bits;
    let free_inodes = INODES_COUNT - 1; // only root (ino 2) is used

    // ── Superblock (lives at block FIRST_DATA_BLOCK, i.e. byte 1024) ──
    let mut sb = alloc::vec![0u8; BLOCK_SIZE as usize];
    sb[0..4].copy_from_slice(&INODES_COUNT.to_le_bytes());
    sb[4..8].copy_from_slice(&TOTAL_BLOCKS.to_le_bytes());
    sb[12..16].copy_from_slice(&free_blocks.to_le_bytes());
    sb[16..20].copy_from_slice(&free_inodes.to_le_bytes());
    sb[20..24].copy_from_slice(&FIRST_DATA_BLOCK.to_le_bytes());
    sb[24..28].copy_from_slice(&0u32.to_le_bytes()); // s_log_block_size = 0 -> 1024-byte blocks
    sb[32..36].copy_from_slice(&blocks_per_group.to_le_bytes());
    sb[40..44].copy_from_slice(&INODES_COUNT.to_le_bytes()); // inodes_per_group (1 group)
    sb[56..58].copy_from_slice(&EXT2_MAGIC.to_le_bytes());
    // s_rev_level (offset 76) left at 0 (rev 0) — see doc comment above for
    // why that's the simplest valid choice here.
    put_block(&mut img, FIRST_DATA_BLOCK, &sb);

    // ── Block group descriptor (block BGDT_BLOCK, offset 0 — only 1 group) ──
    let mut bgd = alloc::vec![0u8; 32];
    bgd[0..4].copy_from_slice(&BLOCK_BITMAP_BLOCK.to_le_bytes());
    bgd[4..8].copy_from_slice(&INODE_BITMAP_BLOCK.to_le_bytes());
    bgd[8..12].copy_from_slice(&INODE_TABLE_START.to_le_bytes());
    bgd[12..14].copy_from_slice(&(free_blocks as u16).to_le_bytes());
    bgd[14..16].copy_from_slice(&(free_inodes as u16).to_le_bytes());
    bgd[16..18].copy_from_slice(&1u16.to_le_bytes()); // bg_used_dirs_count (root)
    let mut bgdt_block_buf = alloc::vec![0u8; BLOCK_SIZE as usize];
    bgdt_block_buf[0..32].copy_from_slice(&bgd);
    put_block(&mut img, BGDT_BLOCK, &bgdt_block_buf);

    // ── Block bitmap: bits 0..used_block_bits (blocks FIRST_DATA_BLOCK..=root_data_block) ──
    let mut block_bitmap = alloc::vec![0u8; BLOCK_SIZE as usize];
    for bit in 0..used_block_bits {
        block_bitmap[(bit / 8) as usize] |= 1u8 << (bit % 8);
    }
    put_block(&mut img, BLOCK_BITMAP_BLOCK, &block_bitmap);

    // ── Inode bitmap: only ino 2 (root) is used ──
    let mut inode_bitmap = alloc::vec![0u8; BLOCK_SIZE as usize];
    inode_bitmap[0] |= 1u8 << (ROOT_INO - 1); // ino is 1-based; bit 0 = ino 1
    put_block(&mut img, INODE_BITMAP_BLOCK, &inode_bitmap);

    // ── Inode table: root's record only, everything else zeroed ──
    let mut root_raw = RawInode::zeroed(INODE_SIZE as usize);
    root_raw.set_i_mode(0x4000 | 0o755);
    root_raw.set_links_count(2); // "." + being a directory's own self-link
    root_raw.set_i_block(0, root_data_block);
    root_raw.set_size(BLOCK_SIZE as u64);
    root_raw.set_blocks_512(BLOCK_SIZE / SECTOR_SIZE as u32);

    let index_in_group = ROOT_INO - 1; // 1
    let table_block = INODE_TABLE_START + index_in_group / inodes_per_block;
    let offset_in_block = ((index_in_group % inodes_per_block) * INODE_SIZE) as usize;
    let mut table_block_buf = alloc::vec![0u8; BLOCK_SIZE as usize];
    table_block_buf[offset_in_block..offset_in_block + INODE_SIZE as usize]
        .copy_from_slice(&root_raw.buf);
    put_block(&mut img, table_block, &table_block_buf);

    // ── Root directory data: "." and ".." (both -> ROOT_INO, root has no parent) ──
    let mut root_dir = alloc::vec![0u8; BLOCK_SIZE as usize];
    let dot_len = dirent_len(1);
    write_dirent(&mut root_dir[0..dot_len], ROOT_INO, dot_len as u16, ".", DIR_FILE_TYPE);
    let remaining = BLOCK_SIZE as usize - dot_len;
    write_dirent(&mut root_dir[dot_len..dot_len + remaining], ROOT_INO, remaining as u16, "..", DIR_FILE_TYPE);
    put_block(&mut img, root_data_block, &root_dir);

    img
}

// ── A minimal image with two orphans (and one "phantom") baked in ──────
//
// Backs `hw_tests.rs`'s diagnostic for whether `reclaim_orphans` actually
// closes the gap `e2fsck -fn disk.img` reports against the real disk (an
// inode + a block marked used in the bitmaps with nothing reachable from
// root pointing at them), and this crate's own host-side
// `repair::tests::reclaim_orphans_clears_injected_orphans_...` test —
// without ever touching that disk. Same base layout as
// `build_minimal_image` (see its doc comment for the block map), extended
// with two more inodes/data-blocks marked used in the bitmaps but **not**
// linked from root's directory data — i.e. deliberately orphaned by
// construction, the same shape an interrupted write or an out-of-band
// tool (`debugfs -w`) can leave behind:
//
//   - ino `ORPHAN_FILE_INO` (20): a plain regular file with one data
//     block — the simplest possible orphan.
//   - ino `ORPHAN_DIR_INO` (31 — deliberately the same inode number
//     `e2fsck -fn disk.img` reported disconnected against the real disk,
//     so this reproduces that report's exact shape, not just "some
//     orphan"): a *directory* whose own data block has "." pointing at
//     itself and ".." pointing at root (ino 2), exactly like a real
//     subdirectory — but with no directory entry anywhere under root
//     pointing back at it.
//
// Both orphan inodes/blocks live in table/data regions this image already
// reserves as valid-but-unused (inode table blocks 5..=20, free data
// blocks from 22 on), so no metadata region needs resizing. The
// superblock/BGD free-block/free-inode counters are set to already agree
// with the bitmaps as modified here (root + 2 orphan inodes used; root +
// both orphan data blocks used) — deliberately isolating what's under test
// to `reclaim_orphans` alone; `reconcile_free_counts` already has its own
// coverage via `build_minimal_image`'s normal use elsewhere.
//
// Also bakes in a "phantom" directory (`PHANTOM_DIR_INO`/`PHANTOM_DIR_BLOCK`)
// — real inode-table record + real "."/".."->root data block — but with
// *neither* bitmap bit set, reproducing the actual shape found by
// reproducing the real `debugfs -w`/`mkdir` leak against `disk.img`
// (`ext2fs_mkdir2()` writes the new inode record and its directory data
// block before it ever attempts to link the name into the parent, and its
// EEXIST error path leaves that already-written content behind WITHOUT
// ever marking either bitmap bit — the opposite polarity from a
// crash-interrupted normal allocation, which sets the bitmap bit
// before/while writing content). This is deliberately outside what
// `reclaim_orphans`'s sweep can touch: that sweep only ever *clears* a bit
// that starts out set — a bit that's already clear, no matter what stale
// content sits behind it, is invisible to it by construction, not by
// omission.

/// Regular-file orphan inode number baked into `build_image_with_orphans`:
/// unreachable from root, no directory entry anywhere points at it.
pub const ORPHAN_FILE_INO: u32 = 20;
/// Directory orphan inode number baked into `build_image_with_orphans`:
/// matches the exact inode number from the real `e2fsck -fn disk.img`
/// report (see the module comment above).
pub const ORPHAN_DIR_INO: u32 = 31;
/// Data block backing `ORPHAN_FILE_INO`, marked used in the block bitmap
/// with nothing under root pointing at it.
pub const ORPHAN_FILE_BLOCK: u32 = 22;
/// Data block backing `ORPHAN_DIR_INO`'s own "."/".." directory data.
pub const ORPHAN_DIR_BLOCK: u32 = 23;
/// The *actual* shape found by reproducing the real `debugfs -w mkdir`
/// leak this module's comment on `build_image_with_orphans` describes: a
/// real, fully-formed directory inode (mode/links/block pointer all set, a
/// real "."/".."->root data block written) that is disconnected from root
/// the same way `ORPHAN_DIR_INO` is — but with its inode-bitmap bit and
/// block-bitmap bit both left **clear** (i.e. "free"), not set.
pub const PHANTOM_DIR_INO: u32 = 45;
/// Data block backing `PHANTOM_DIR_INO`'s own "."/".." directory data —
/// real content, bitmap bit left clear (see `PHANTOM_DIR_INO`).
pub const PHANTOM_DIR_BLOCK: u32 = 24;

pub fn build_image_with_orphans() -> Vec<u8> {
    const BLOCK_SIZE: u32 = 1024;
    const TOTAL_BLOCKS: u32 = 256;
    const INODES_COUNT: u32 = 128;
    const INODE_SIZE: u32 = 128;
    const FIRST_DATA_BLOCK: u32 = 1;
    const BGDT_BLOCK: u32 = FIRST_DATA_BLOCK + 1;
    const BLOCK_BITMAP_BLOCK: u32 = 3;
    const INODE_BITMAP_BLOCK: u32 = 4;
    const INODE_TABLE_START: u32 = 5;

    let inodes_per_block = BLOCK_SIZE / INODE_SIZE; // 8
    let inode_table_blocks = (INODES_COUNT + inodes_per_block - 1) / inodes_per_block; // 16
    let root_data_block = INODE_TABLE_START + inode_table_blocks; // 21
    let orphan_file_block = ORPHAN_FILE_BLOCK;
    let orphan_dir_block = ORPHAN_DIR_BLOCK;
    debug_assert_eq!(orphan_file_block, root_data_block + 1);
    debug_assert_eq!(orphan_dir_block, root_data_block + 2);

    let mut img = alloc::vec![0u8; (TOTAL_BLOCKS * BLOCK_SIZE) as usize];
    let put_block = |img: &mut Vec<u8>, block_num: u32, data: &[u8]| {
        let off = (block_num * BLOCK_SIZE) as usize;
        img[off..off + data.len()].copy_from_slice(data);
    };

    // root's own metadata footprint (same as build_minimal_image), plus
    // the two orphan data blocks.
    let used_block_bits = root_data_block - FIRST_DATA_BLOCK + 1; // 21
    let blocks_per_group = TOTAL_BLOCKS;
    let blocks_in_group0 = TOTAL_BLOCKS - FIRST_DATA_BLOCK; // 255
    let free_blocks = blocks_in_group0 - used_block_bits - 2; // minus both orphan blocks
    let free_inodes = INODES_COUNT - 1 - 2; // minus root and both orphan inodes

    // ── Superblock ──
    let mut sb = alloc::vec![0u8; BLOCK_SIZE as usize];
    sb[0..4].copy_from_slice(&INODES_COUNT.to_le_bytes());
    sb[4..8].copy_from_slice(&TOTAL_BLOCKS.to_le_bytes());
    sb[12..16].copy_from_slice(&free_blocks.to_le_bytes());
    sb[16..20].copy_from_slice(&free_inodes.to_le_bytes());
    sb[20..24].copy_from_slice(&FIRST_DATA_BLOCK.to_le_bytes());
    sb[24..28].copy_from_slice(&0u32.to_le_bytes());
    sb[32..36].copy_from_slice(&blocks_per_group.to_le_bytes());
    sb[40..44].copy_from_slice(&INODES_COUNT.to_le_bytes());
    sb[56..58].copy_from_slice(&EXT2_MAGIC.to_le_bytes());
    put_block(&mut img, FIRST_DATA_BLOCK, &sb);

    // ── Block group descriptor ──
    let mut bgd = alloc::vec![0u8; 32];
    bgd[0..4].copy_from_slice(&BLOCK_BITMAP_BLOCK.to_le_bytes());
    bgd[4..8].copy_from_slice(&INODE_BITMAP_BLOCK.to_le_bytes());
    bgd[8..12].copy_from_slice(&INODE_TABLE_START.to_le_bytes());
    bgd[12..14].copy_from_slice(&(free_blocks as u16).to_le_bytes());
    bgd[14..16].copy_from_slice(&(free_inodes as u16).to_le_bytes());
    // bg_used_dirs_count: root + the orphan directory (it IS a directory
    // on disk, even though nothing links to it — a real mke2fs/e2fsck
    // would count it here too).
    bgd[16..18].copy_from_slice(&2u16.to_le_bytes());
    let mut bgdt_block_buf = alloc::vec![0u8; BLOCK_SIZE as usize];
    bgdt_block_buf[0..32].copy_from_slice(&bgd);
    put_block(&mut img, BGDT_BLOCK, &bgdt_block_buf);

    // ── Block bitmap: metadata footprint + both orphan data blocks ──
    let mut block_bitmap = alloc::vec![0u8; BLOCK_SIZE as usize];
    for bit in 0..used_block_bits {
        block_bitmap[(bit / 8) as usize] |= 1u8 << (bit % 8);
    }
    mark_bit(&mut block_bitmap, orphan_file_block - FIRST_DATA_BLOCK);
    mark_bit(&mut block_bitmap, orphan_dir_block - FIRST_DATA_BLOCK);
    put_block(&mut img, BLOCK_BITMAP_BLOCK, &block_bitmap);

    // ── Inode bitmap: root + both orphans ──
    let mut inode_bitmap = alloc::vec![0u8; BLOCK_SIZE as usize];
    inode_bitmap[0] |= 1u8 << (ROOT_INO - 1);
    mark_bit_1based(&mut inode_bitmap, ORPHAN_FILE_INO);
    mark_bit_1based(&mut inode_bitmap, ORPHAN_DIR_INO);
    put_block(&mut img, INODE_BITMAP_BLOCK, &inode_bitmap);

    // Helper: write one inode record into whatever inode-table block it
    // belongs to (each orphan lands in its own previously-all-zero table
    // block here, so a single put_block of that whole block is enough —
    // same pattern build_minimal_image uses for root's own record).
    let write_inode_record = |img: &mut Vec<u8>, ino: u32, raw: &RawInode| {
        let index_in_group = ino - 1;
        let table_block = INODE_TABLE_START + index_in_group / inodes_per_block;
        let offset_in_block = ((index_in_group % inodes_per_block) * INODE_SIZE) as usize;
        let mut table_block_buf = alloc::vec![0u8; BLOCK_SIZE as usize];
        table_block_buf[offset_in_block..offset_in_block + INODE_SIZE as usize]
            .copy_from_slice(&raw.buf);
        put_block(img, table_block, &table_block_buf);
    };

    // ── Root's own inode + directory data (identical to build_minimal_image) ──
    let mut root_raw = RawInode::zeroed(INODE_SIZE as usize);
    root_raw.set_i_mode(0x4000 | 0o755);
    root_raw.set_links_count(2);
    root_raw.set_i_block(0, root_data_block);
    root_raw.set_size(BLOCK_SIZE as u64);
    root_raw.set_blocks_512(BLOCK_SIZE / SECTOR_SIZE as u32);
    write_inode_record(&mut img, ROOT_INO, &root_raw);

    let mut root_dir = alloc::vec![0u8; BLOCK_SIZE as usize];
    let dot_len = dirent_len(1);
    write_dirent(&mut root_dir[0..dot_len], ROOT_INO, dot_len as u16, ".", DIR_FILE_TYPE);
    let remaining = BLOCK_SIZE as usize - dot_len;
    write_dirent(&mut root_dir[dot_len..dot_len + remaining], ROOT_INO, remaining as u16, "..", DIR_FILE_TYPE);
    // Deliberately NOT adding entries for either orphan here — that
    // omission is exactly what makes them orphans.
    put_block(&mut img, root_data_block, &root_dir);

    // ── Orphan regular file: ino 20, one data block, no directory entry ──
    let mut file_raw = RawInode::zeroed(INODE_SIZE as usize);
    file_raw.set_i_mode(0x8000 | 0o644);
    file_raw.set_links_count(1);
    file_raw.set_i_block(0, orphan_file_block);
    file_raw.set_size(BLOCK_SIZE as u64);
    file_raw.set_blocks_512(BLOCK_SIZE / SECTOR_SIZE as u32);
    write_inode_record(&mut img, ORPHAN_FILE_INO, &file_raw);
    // Data block content is irrelevant to the reachability question —
    // leave it zeroed (already is).

    // ── Orphan directory: ino 31, "." -> self, ".." -> root, no entry
    // anywhere under root pointing at it ──
    let mut dir_raw = RawInode::zeroed(INODE_SIZE as usize);
    dir_raw.set_i_mode(0x4000 | 0o755);
    dir_raw.set_links_count(2); // "." + its own directory-ness, same as root
    dir_raw.set_i_block(0, orphan_dir_block);
    dir_raw.set_size(BLOCK_SIZE as u64);
    dir_raw.set_blocks_512(BLOCK_SIZE / SECTOR_SIZE as u32);
    write_inode_record(&mut img, ORPHAN_DIR_INO, &dir_raw);

    let mut orphan_dir_data = alloc::vec![0u8; BLOCK_SIZE as usize];
    write_dirent(&mut orphan_dir_data[0..dot_len], ORPHAN_DIR_INO, dot_len as u16, ".", DIR_FILE_TYPE);
    write_dirent(&mut orphan_dir_data[dot_len..dot_len + remaining], ROOT_INO, remaining as u16, "..", DIR_FILE_TYPE);
    put_block(&mut img, orphan_dir_block, &orphan_dir_data);

    // ── Phantom directory: same disconnected shape as ORPHAN_DIR_INO, but
    // neither bitmap bit is ever set — see the module comment above ──
    let mut phantom_raw = RawInode::zeroed(INODE_SIZE as usize);
    phantom_raw.set_i_mode(0x4000 | 0o755);
    phantom_raw.set_links_count(2);
    phantom_raw.set_i_block(0, PHANTOM_DIR_BLOCK);
    phantom_raw.set_size(BLOCK_SIZE as u64);
    phantom_raw.set_blocks_512(BLOCK_SIZE / SECTOR_SIZE as u32);
    write_inode_record(&mut img, PHANTOM_DIR_INO, &phantom_raw);

    let mut phantom_dir_data = alloc::vec![0u8; BLOCK_SIZE as usize];
    write_dirent(&mut phantom_dir_data[0..dot_len], PHANTOM_DIR_INO, dot_len as u16, ".", DIR_FILE_TYPE);
    write_dirent(&mut phantom_dir_data[dot_len..dot_len + remaining], ROOT_INO, remaining as u16, "..", DIR_FILE_TYPE);
    put_block(&mut img, PHANTOM_DIR_BLOCK, &phantom_dir_data);
    // Deliberately NOT marking PHANTOM_DIR_INO/PHANTOM_DIR_BLOCK used in
    // either bitmap, and NOT accounted for in free_blocks/free_inodes
    // above — the bitmaps already (accidentally) agree this is "free",
    // which is exactly the point.

    img
}
