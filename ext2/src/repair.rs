// ext2/src/repair.rs
//
// Mount-time consistency repair — migration step 5
// (`docs/fs/ext2-extraction-plan.md`). Moved verbatim out of
// `kernel::fs::ext2::Ext2Fs` — same on-disk format, same write ordering,
// same walk order, same error conditions. See `CLAUDE.md`'s "Filesystem:
// ext2" section for the wider story (why this exists, no journal, crash
// consistency), and in particular its "Critical ordering invariant in
// reclaim_orphans" paragraph, reproduced verbatim on `reclaim_orphans`
// below — that ordering already corrupted a fresh mount once and the
// comment is what stops it happening again, not just documentation.
//
// This module cannot call `crate::ktrace!`/`crate::debug` the way the
// pre-extraction kernel code did — `ext2` doesn't depend on the kernel, and
// moving that tracing infra here would be a much bigger change than this
// step's "no behavior change" scope allows (see the crate doc comment's
// "Error type and locking" section for the same reasoning applied to
// `Errno`). So `reconcile_free_counts`/`reclaim_orphans` below report what
// they found/fixed through their return values instead of tracing
// themselves; `kernel::fs::ext2::Ext2Fs::reconcile_free_counts`/
// `reclaim_orphans` (thin wrappers over these) inspect that return value
// and emit the `ktrace!` line + the permanent `/proc/kdebug` counter
// (`crate::debug::add_orphans_reclaimed`) themselves. The kernel's original
// `reconcile_free_counts` traced per-group drift individually (group
// number, before/after block/inode counts) — that level of detail is
// diagnostic-only (gated off by default, off unless `kdebug fs on`), so
// `ReconcileReport` below collapses it to "did the BGDs/superblock need
// correcting" plus the final corrected totals; nothing about the *repair
// itself* (which bits get cleared, which counters get adjusted, in what
// order) changed, only how the fact of it gets reported upward.

use crate::bitmap::count_free_bits;
use crate::error::Ext2Error;
use crate::superblock::ROOT_INO;
use crate::volume::Ext2Core;

/// Outcome of [`Ext2Core::reconcile_free_counts`] — enough for the kernel
/// adapter to emit one summarized trace line without this crate needing to
/// know about `ktrace!`. See this module's doc comment for why the
/// original per-group trace detail was collapsed rather than carried
/// across the crate boundary; a caller that wants the per-group detail
/// back can always re-derive it with [`Ext2Core::bgd_free_counts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Whether any block group descriptor's free block/inode counters
    /// disagreed with its bitmap and needed correcting.
    pub bgd_drift: bool,
    /// Whether the superblock's own free block/inode counters disagreed
    /// with the true bitmap-derived totals and needed correcting.
    pub sb_drift: bool,
    /// True total free blocks across every group, after any repair.
    pub total_free_blocks: u32,
    /// True total free inodes across every group, after any repair.
    pub total_free_inodes: u32,
}

impl Ext2Core {
    // ── Mount-time consistency repair ───────────────────────────────────

    /// Recompute every group's true free block/inode counts directly from
    /// its bitmap and correct the stored BGD + superblock counters if they
    /// disagree. Called once from the kernel adapter's `mount_and_repair`,
    /// before this filesystem is exposed to the VFS.
    ///
    /// Bitmap writes are always durable the instant they happen (this
    /// driver has no write-back cache), but the *counters* that track free
    /// space are separate, independently-flushed writes (see the crate/
    /// module doc comments) — an unclean shutdown between a bitmap write
    /// and its matching counter update leaves the bitmap correct but the
    /// counter stale. Left unrepaired, that drift is a real correctness
    /// bug, not just cosmetic: `alloc_block`/`alloc_inode` use the counter
    /// as a fast "is this group full" pre-check, so a counter that's stuck
    /// too low makes them wrongly skip a group that actually has free
    /// bits, eventually surfacing as spurious `ENOSPC`. This is the same
    /// repair real `e2fsck` applies most often in practice; it does not
    /// attempt the harder problem of reclaiming blocks/inodes that a crash
    /// left allocated-but-unlinked (an orphan scan needs a full
    /// reachability walk from the root) — that's `reclaim_orphans` below.
    pub fn reconcile_free_counts(&self) -> Result<ReconcileReport, Ext2Error> {
        let mut sb_raw = [0u8; 1024];
        self.device.read_sectors(2, 2, &mut sb_raw).map_err(|_| Ext2Error::Io)?;
        let sb_free_blocks = u32::from_le_bytes(sb_raw[12..16].try_into().unwrap());
        let sb_free_inodes = u32::from_le_bytes(sb_raw[16..20].try_into().unwrap());

        let mut total_free_blocks: u32 = 0;
        let mut total_free_inodes: u32 = 0;
        let mut bgd_drift = false;

        for group in 0..self.sb.num_groups {
            let bgd = self.read_bgd(group)?;

            let block_bitmap = self.block_vec(bgd.block_bitmap)?;
            let real_free_blocks = count_free_bits(&block_bitmap, self.blocks_in_group(group));

            let inode_bitmap = self.block_vec(bgd.inode_bitmap)?;
            let real_free_inodes = count_free_bits(&inode_bitmap, self.inodes_in_group(group));

            if real_free_blocks != bgd.free_blocks || real_free_inodes != bgd.free_inodes {
                bgd_drift = true;
                self.adjust_bgd_counts(
                    group,
                    real_free_blocks as i32 - bgd.free_blocks as i32,
                    real_free_inodes as i32 - bgd.free_inodes as i32,
                    0,
                )?;
            }

            total_free_blocks += real_free_blocks as u32;
            total_free_inodes += real_free_inodes as u32;
        }

        let sb_drift = total_free_blocks != sb_free_blocks || total_free_inodes != sb_free_inodes;
        if sb_drift {
            self.adjust_sb_counts(
                total_free_blocks as i32 - sb_free_blocks as i32,
                total_free_inodes as i32 - sb_free_inodes as i32,
            )?;
        }

        Ok(ReconcileReport { bgd_drift, sb_drift, total_free_blocks, total_free_inodes })
    }

    /// Mount-time orphan scan — see the crate/module doc comments for the
    /// full rationale. Builds a "should be used" bitmap pair by walking
    /// every inode actually reachable from the root directory (fixed
    /// metadata + reserved inodes are seeded in as used up front, same
    /// convention real ext2 tools use), then clears any bit the real
    /// on-disk bitmaps mark used that the walk never reached. Returns
    /// `(freed_blocks, freed_inodes)` — the kernel adapter traces/counts
    /// these itself (see module doc comment) rather than this crate doing
    /// it directly.
    ///
    /// Safety-critical property: the sweep only ever runs if the walk
    /// completed with no error at all (`?` on every fallible step here
    /// means a single I/O failure or a directory tree deeper than the
    /// depth guard aborts the *whole* function before the sweep, via
    /// `mark_reachable`'s own `Err` return — never partially). An
    /// incomplete "should be used" picture must never be swept against,
    /// or a still-live block/inode could be freed out from under a file
    /// that's simply reached through a deep path.
    pub fn reclaim_orphans(&self) -> Result<(u32, u32), Ext2Error> {
        let block_bytes = ((self.sb.blocks_count as usize) + 7) / 8;
        let inode_bytes = ((self.sb.inodes_count as usize) + 7) / 8;
        let mut used_blocks = alloc::vec![0u8; block_bytes];
        let mut used_inodes = alloc::vec![0u8; inode_bytes];

        // Fixed metadata: boot block, the superblock itself, the
        // block-group descriptor table, and every group's own bitmaps +
        // inode table. None of this is owned by any inode, so the tree
        // walk below would never mark it, but it's legitimately in use.
        // The superblock lives AT block `first_data_block` (`bgdt_block`
        // above is computed as `first_data_block + 1`, i.e. right after
        // it) — `0..first_data_block` only covers the boot block ahead of
        // it, not the superblock's own block, so that block must be
        // included too (`0..=first_data_block`). Missing this let the
        // sweep below "reclaim" the superblock's block as an orphan on
        // first mount, and the very next allocation handed it out to real
        // file data, corrupting the superblock.
        for b in 0..=self.sb.first_data_block {
            mark_bit(&mut used_blocks, b);
        }
        let bgd_per_block = self.sb.block_size / 32;
        let bgdt_blocks = (self.sb.num_groups + bgd_per_block - 1) / bgd_per_block;
        for b in self.sb.bgdt_block..self.sb.bgdt_block + bgdt_blocks {
            mark_bit(&mut used_blocks, b);
        }
        let inodes_per_block = self.sb.block_size / self.sb.inode_size as u32;
        for group in 0..self.sb.num_groups {
            let bgd = self.read_bgd(group)?;
            mark_bit(&mut used_blocks, bgd.block_bitmap);
            mark_bit(&mut used_blocks, bgd.inode_bitmap);
            let inode_table_blocks = (self.sb.inodes_per_group + inodes_per_block - 1) / inodes_per_block;
            for b in bgd.inode_table..bgd.inode_table + inode_table_blocks {
                mark_bit(&mut used_blocks, b);
            }

            // Real ext2 (sparse_super, mke2fs's default) keeps backup
            // superblock+BGDT copies in group 0, group 1, and every group
            // whose number is a power of 3/5/7 — mirroring that placement
            // logic exactly here would be one more way to get it subtly
            // wrong, so instead just reserve every group's leading
            // `1 + bgdt_blocks` blocks unconditionally. Overkill for a
            // group that doesn't actually have a backup (those blocks were
            // never marked used in the real per-group bitmap to begin
            // with, so reserving them here is a no-op), but it means a
            // group that *does* have one — which this driver has no
            // reason to expect specifically — can never be misread as an
            // orphan and freed into real file data the way group 0's own
            // primary copy already was (see the loop above this one).
            let group_start = self.sb.first_data_block + group * self.sb.blocks_per_group;
            for b in group_start..group_start + 1 + bgdt_blocks {
                mark_bit(&mut used_blocks, b);
            }
        }

        // Walk the real directory tree from root, marking every inode and
        // block actually reachable. 64 levels of nesting is far beyond
        // anything a shell/script here would ever create — hitting it is
        // treated as a hard error (not a silent stop), since silently
        // under-marking a legitimately-deep subtree would make the sweep
        // below wrongly reclaim it.
        //
        // MUST run before the reserved-inode marking just below: root's
        // own ino (2) falls inside that reserved range, and
        // `mark_reachable`'s cycle guard treats an already-marked bit as
        // "already visited, nothing more to do" — pre-marking root first
        // used to make the very first call return immediately without
        // ever reading root's own blocks or descending into a single
        // child. That silently treated the *entire* real directory tree
        // (every file this filesystem was seeded with) as unreachable, so
        // the sweep below freed almost every real block/inode on the very
        // first mount — the very first new file/dir write after that
        // then handed out an already-live block to something else,
        // corrupting whatever legitimately owned it (this is what
        // produced the `add_dir_entry` "range end index ... out of range"
        // panic: the root directory's own data block had been reused for
        // unrelated file content).
        self.mark_reachable(ROOT_INO, &mut used_inodes, &mut used_blocks, 64)?;

        // Reserved inodes below `first_ino` (root's own ino=2 among them)
        // are always "in use" even though this driver never reaches most
        // of them (1, 3..=10 have no directory entry pointing at them at
        // all, ever) via the walk above.
        for ino in 1..self.sb.first_ino {
            mark_bit_1based(&mut used_inodes, ino);
        }

        // Sweep: anything the real bitmaps mark used that the walk above
        // never reached is an orphan — clear it. Counter bookkeeping is
        // deliberately NOT duplicated here: `reconcile_free_counts` (just
        // above) already knows how to recompute BGD/superblock free
        // counts from a bitmap, so just clear bits and re-run it once at
        // the end if anything actually changed.
        let mut freed_blocks: u32 = 0;
        let mut freed_inodes: u32 = 0;
        for group in 0..self.sb.num_groups {
            let bgd = self.read_bgd(group)?;

            let mut block_bitmap = self.block_vec(bgd.block_bitmap)?;
            let mut changed = false;
            for bit in 0..self.blocks_in_group(group) {
                let byte = (bit / 8) as usize;
                let mask = 1u8 << (bit % 8);
                if block_bitmap[byte] & mask == 0 {
                    continue;
                }
                let block_num = self.sb.first_data_block + group * self.sb.blocks_per_group + bit;
                if !bit_set(&used_blocks, block_num) {
                    block_bitmap[byte] &= !mask;
                    changed = true;
                    freed_blocks += 1;
                }
            }
            if changed {
                self.write_block(bgd.block_bitmap, &block_bitmap)?;
            }

            let mut inode_bitmap = self.block_vec(bgd.inode_bitmap)?;
            let mut ichanged = false;
            for bit in 0..self.inodes_in_group(group) {
                let byte = (bit / 8) as usize;
                let mask = 1u8 << (bit % 8);
                if inode_bitmap[byte] & mask == 0 {
                    continue;
                }
                let ino = group * self.sb.inodes_per_group + bit + 1;
                if !bit_set_1based(&used_inodes, ino) {
                    inode_bitmap[byte] &= !mask;
                    ichanged = true;
                    freed_inodes += 1;
                }
            }
            if ichanged {
                self.write_block(bgd.inode_bitmap, &inode_bitmap)?;
            }
        }

        if freed_blocks > 0 || freed_inodes > 0 {
            // Re-run the free-count reconciliation above so the BGD/
            // superblock counters agree with the bits this sweep just
            // cleared — same "clear bits, then reconcile once" split the
            // pre-extraction kernel code used. The kernel adapter is the
            // one that traces/counts `freed_blocks`/`freed_inodes` (see
            // this module's doc comment); this call's own `ReconcileReport`
            // is intentionally discarded — nothing here needs it.
            self.reconcile_free_counts()?;
        }

        Ok((freed_blocks, freed_inodes))
    }

    /// Recursive step of `reclaim_orphans`: mark `ino` (and, if it's a
    /// directory, everything reachable through it) as used in
    /// `used_inodes`/`used_blocks`. `used_inodes` doubles as the
    /// already-visited set — an inode marked on entry short-circuits
    /// immediately, which is what makes a cyclic (corrupted) directory
    /// structure terminate instead of recursing forever, on top of the
    /// hard `depth_left` bound below.
    fn mark_reachable(&self, ino: u32, used_inodes: &mut [u8], used_blocks: &mut [u8], depth_left: u32) -> Result<(), Ext2Error> {
        if bit_set_1based(used_inodes, ino) {
            return Ok(()); // already visited — cycle guard
        }
        if depth_left == 0 {
            // See `reclaim_orphans`'s doc comment: failing loudly here
            // (instead of silently stopping) is what keeps an
            // unexpectedly-deep-but-legitimate subtree from being
            // mistaken for garbage by the sweep.
            return Err(Ext2Error::TooDeep);
        }
        mark_bit_1based(used_inodes, ino);

        let raw = self.read_inode(ino)?;
        if raw.has_block_pointers() {
            self.visit_inode_blocks(&raw, |b| {
                mark_bit(used_blocks, b);
                Ok(())
            })?;
        }

        if raw.is_dir() {
            for entry in self.read_dir_entries(&raw)? {
                self.mark_reachable(entry.ino, used_inodes, used_blocks, depth_left - 1)?;
            }
        }

        Ok(())
    }

    // ── Test/inspection accessors ───────────────────────────────────────
    //
    // Not gated `#[cfg(test)]`: cheap, read-only bitmap/inode inspection
    // with no on-disk side effects, kept as ordinary `pub fn`s so both this
    // crate's own host tests (`repair::tests` below) and the kernel's
    // `#[cfg(test)]`-only `TestFs` wrapper (`kernel/src/fs/ext2.rs`, which
    // `kernel/src/hw_tests.rs`'s QEMU integration test drives) can call
    // them directly instead of each keeping its own copy. Moved verbatim
    // out of what used to be `kernel::fs::ext2::TestFs`'s own methods.

    /// Whether `ino` (1-based) is marked used in its group's on-disk
    /// inode bitmap right now.
    pub fn inode_used(&self, ino: u32) -> Result<bool, Ext2Error> {
        let group = (ino - 1) / self.sb.inodes_per_group;
        let bit = (ino - 1) % self.sb.inodes_per_group;
        let bgd = self.read_bgd(group)?;
        let bitmap = self.block_vec(bgd.inode_bitmap)?;
        Ok(bit_set(&bitmap, bit))
    }

    /// Whether `block` (absolute block number) is marked used in its
    /// group's on-disk block bitmap right now.
    pub fn block_used(&self, block: u32) -> Result<bool, Ext2Error> {
        let group = (block - self.sb.first_data_block) / self.sb.blocks_per_group;
        let bit = (block - self.sb.first_data_block) % self.sb.blocks_per_group;
        let bgd = self.read_bgd(group)?;
        let bitmap = self.block_vec(bgd.block_bitmap)?;
        Ok(bit_set(&bitmap, bit))
    }

    /// Raw `i_mode` of `ino`'s on-disk inode record, read directly (not
    /// gated on the bitmap at all) — lets a test prove a "phantom" inode's
    /// real content (mode/links/block pointer) is still sitting there
    /// untouched after a repair pass that, by design, never looks at
    /// content behind an already-clear bitmap bit.
    pub fn inode_mode(&self, ino: u32) -> Result<u16, Ext2Error> {
        Ok(self.read_inode(ino)?.i_mode())
    }

    /// `(free_blocks, free_inodes)` straight off the on-disk superblock.
    pub fn sb_free_counts(&self) -> Result<(u32, u32), Ext2Error> {
        let mut raw = [0u8; 1024];
        self.device.read_sectors(2, 2, &mut raw).map_err(|_| Ext2Error::Io)?;
        let free_blocks = u32::from_le_bytes(raw[12..16].try_into().unwrap());
        let free_inodes = u32::from_le_bytes(raw[16..20].try_into().unwrap());
        Ok((free_blocks, free_inodes))
    }

    /// `(free_blocks, free_inodes)` straight off group `group`'s on-disk
    /// BGD entry.
    pub fn bgd_free_counts(&self, group: u32) -> Result<(u16, u16), Ext2Error> {
        let bgd = self.read_bgd(group)?;
        Ok((bgd.free_blocks, bgd.free_inodes))
    }

    /// Recompute the *true* free block/inode counts directly from the
    /// bitmaps (same formula `reconcile_free_counts` uses internally) —
    /// lets a test assert the on-disk counters are self-consistent with
    /// the bitmaps, not just with what the test expects.
    pub fn true_free_counts_group0(&self) -> Result<(u16, u16), Ext2Error> {
        let bgd = self.read_bgd(0)?;
        let block_bitmap = self.block_vec(bgd.block_bitmap)?;
        let real_free_blocks = count_free_bits(&block_bitmap, self.blocks_in_group(0));
        let inode_bitmap = self.block_vec(bgd.inode_bitmap)?;
        let real_free_inodes = count_free_bits(&inode_bitmap, self.inodes_in_group(0));
        Ok((real_free_blocks, real_free_inodes))
    }
}

/// Set bit `n` (0-based) in a byte-packed bitmap that spans the *whole*
/// filesystem's block/inode range — unlike `bitmap::set_bit` (which trusts
/// its caller and panics out of range), this silently ignores an
/// out-of-range `n`. `reclaim_orphans` marks bits straight out of on-disk
/// `i_block` pointers via `visit_inode_blocks`, which does not itself
/// bounds-check against `used_blocks`' length before calling back here — a
/// corrupted pointer must not panic the whole repair pass.
pub(crate) fn mark_bit(bitmap: &mut [u8], n: u32) {
    let byte = (n / 8) as usize;
    if byte < bitmap.len() {
        bitmap[byte] |= 1u8 << (n % 8);
    }
}

/// Whether bit `n` (0-based) is set — same out-of-range-is-false contract
/// as `bitmap::bit_is_set` (this is, in fact, the exact same logic; kept
/// as its own local copy rather than a re-export purely so this file reads
/// standalone next to `mark_bit`, which cannot be a re-export of anything
/// in `bitmap.rs` since that module's `set_bit` panics out of range).
fn bit_set(bitmap: &[u8], n: u32) -> bool {
    let byte = (n / 8) as usize;
    bitmap.get(byte).is_some_and(|b| b & (1u8 << (n % 8)) != 0)
}

/// Same as `mark_bit`/`bit_set`, but for a 1-based inode number (real
/// ext2's own convention — inode 0 doesn't exist, inode 1 is bit 0).
pub(crate) fn mark_bit_1based(bitmap: &mut [u8], ino: u32) {
    if ino >= 1 {
        mark_bit(bitmap, ino - 1);
    }
}

fn bit_set_1based(bitmap: &[u8], ino: u32) -> bool {
    ino >= 1 && bit_set(bitmap, ino - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{build_image_with_orphans, mount, ORPHAN_DIR_BLOCK, ORPHAN_DIR_INO,
        ORPHAN_FILE_BLOCK, ORPHAN_FILE_INO, PHANTOM_DIR_BLOCK, PHANTOM_DIR_INO};
    use hal::block::MemDisk;

    /// `reconcile_free_counts` against an already-consistent image (the
    /// plain `minimal_image()` fixture, untouched) must report no drift
    /// and leave every counter exactly as it found it.
    #[test]
    fn reconcile_free_counts_reports_no_drift_on_a_consistent_image() {
        let core = mount(crate::test_support::minimal_image());
        let report = core.reconcile_free_counts().expect("reconcile");
        assert!(!report.bgd_drift);
        assert!(!report.sb_drift);
    }

    /// Desync a group's BGD free-block counter from its bitmap by hand
    /// (simulating the unclean-shutdown drift `reconcile_free_counts`
    /// exists to fix), then confirm the repair pass corrects it and
    /// reports that it did.
    #[test]
    fn reconcile_free_counts_repairs_bgd_drift() {
        let core = mount(crate::test_support::minimal_image());
        // Corrupt the stored free-block counter without touching the real
        // bitmap — exactly the "bitmap correct, counter stale" drift shape.
        core.adjust_bgd_counts(0, -5, 0, 0).unwrap();
        core.adjust_sb_counts(-5, 0).unwrap();

        let before = core.bgd_free_counts(0).unwrap();
        let report = core.reconcile_free_counts().expect("reconcile");
        assert!(report.bgd_drift, "drift must be detected");
        let after = core.bgd_free_counts(0).unwrap();
        assert_eq!(after.0, before.0 + 5, "counter must be corrected back to the real bitmap value");
    }

    /// Full end-to-end reproduction of the real `hw_tests.rs`
    /// (`ext2_reclaim_orphans_clears_injected_disk_img_shape`) scenario,
    /// against the exact same injected-orphan image shape, run purely on
    /// the host — the byte-exact correctness oracle for `reclaim_orphans`
    /// (see the `e2fsck`-oracle test further down for why a real `e2fsck`
    /// run is deliberately NOT attached to this particular test).
    #[test]
    fn reclaim_orphans_clears_injected_orphans_but_leaves_root_and_phantom_alone() {
        let core = mount(MemDisk::from_vec(build_image_with_orphans()));

        // Sanity: orphans start marked used, phantom starts marked free
        // despite having real content on disk.
        assert!(core.inode_used(ORPHAN_FILE_INO).unwrap());
        assert!(core.block_used(ORPHAN_FILE_BLOCK).unwrap());
        assert!(core.inode_used(ORPHAN_DIR_INO).unwrap());
        assert!(core.block_used(ORPHAN_DIR_BLOCK).unwrap());
        assert!(!core.inode_used(PHANTOM_DIR_INO).unwrap());
        assert!(!core.block_used(PHANTOM_DIR_BLOCK).unwrap());
        let phantom_mode_before = core.inode_mode(PHANTOM_DIR_INO).unwrap();

        core.reconcile_free_counts().expect("reconcile against a self-consistent image");
        let (sb_free_blocks_before, sb_free_inodes_before) = core.sb_free_counts().unwrap();
        let (true_free_blocks_before, true_free_inodes_before) = core.true_free_counts_group0().unwrap();
        assert_eq!(sb_free_blocks_before, true_free_blocks_before as u32);
        assert_eq!(sb_free_inodes_before, true_free_inodes_before as u32);

        let (freed_blocks, freed_inodes) = core.reclaim_orphans().expect("reclaim_orphans");
        assert_eq!(freed_blocks, 2, "both orphan data blocks must be reclaimed");
        assert_eq!(freed_inodes, 2, "both orphan inodes must be reclaimed");

        assert!(!core.inode_used(ORPHAN_FILE_INO).unwrap());
        assert!(!core.block_used(ORPHAN_FILE_BLOCK).unwrap());
        assert!(!core.inode_used(ORPHAN_DIR_INO).unwrap());
        assert!(!core.block_used(ORPHAN_DIR_BLOCK).unwrap());

        // Root itself must never be swept.
        assert!(core.inode_used(ROOT_INO).unwrap());
        assert!(core.block_used(21).unwrap()); // root's own directory data block

        // Counters must stay self-consistent after the sweep.
        let (sb_free_blocks_after, sb_free_inodes_after) = core.sb_free_counts().unwrap();
        let (bgd_free_blocks_after, bgd_free_inodes_after) = core.bgd_free_counts(0).unwrap();
        let (true_free_blocks_after, true_free_inodes_after) = core.true_free_counts_group0().unwrap();
        assert_eq!(sb_free_blocks_after, true_free_blocks_after as u32);
        assert_eq!(sb_free_inodes_after, true_free_inodes_after as u32);
        assert_eq!(bgd_free_blocks_after, true_free_blocks_after);
        assert_eq!(bgd_free_inodes_after, true_free_inodes_after);
        assert_eq!(sb_free_blocks_after, sb_free_blocks_before + 2);
        assert_eq!(sb_free_inodes_after, sb_free_inodes_before + 2);

        // Phantom (bitmap bit never set to begin with) is out of scope for
        // the sweep by construction — must survive completely untouched.
        assert!(!core.inode_used(PHANTOM_DIR_INO).unwrap());
        assert!(!core.block_used(PHANTOM_DIR_BLOCK).unwrap());
        assert_eq!(core.inode_mode(PHANTOM_DIR_INO).unwrap(), phantom_mode_before);
    }

    // ── e2fsck oracle ────────────────────────────────────────────────────
    //
    // `docs/fs/ext2-extraction-plan.md` calls a real `e2fsck -fn` verdict
    // "the actual point" of this whole extraction. Getting one requires a
    // filesystem image `e2fsck` itself is willing to open at all — and,
    // empirically (confirmed while building this test, independent of
    // anything under test here), NEITHER of this crate's own hand-built
    // fixtures (`minimal_image`/`build_image_with_orphans`) qualifies:
    // `e2fsck -fn` refuses even the freshly-mounted, untouched bytes with
    // "ext2fs_open2(): El superbloque ext2 está corrupto". Real `mke2fs`
    // sets several superblock fields this crate's own parser never needed
    // to reproduce (recorded timestamps, reserved-block accounting,
    // `s_blocks_per_group` sized off the block size rather than the
    // device's actual block count, ...) and `e2fsck`'s own sanity gate
    // (`check_super_value`) is strict about them — nothing to do with
    // `reconcile_free_counts`/`reclaim_orphans` at all. So the oracle test
    // below builds a *real* `mke2fs` image at test time instead.
    //
    // `reconcile_free_counts` gets a real, passing oracle test: it only
    // ever touches free-count bookkeeping, never bitmaps or inode records,
    // so repairing pure counter drift on top of an otherwise-untouched
    // real `mke2fs` image is exactly the kind of fix `e2fsck -fn` confirms
    // clean.
    //
    // `reclaim_orphans` deliberately does NOT get an equivalent "exit 0"
    // oracle test. Building a real `mke2fs` + `debugfs -w` orphan fixture
    // during this migration (`mkdir` two directories, then `debugfs`'s own
    // `unlink` command — "does not adjust the inode reference counts, so
    // this can be used to create an orphan inode by hand", i.e. exactly
    // this driver's target shape) and running the full repair pipeline
    // against it surfaced a genuine, pre-existing property of
    // `reclaim_orphans`'s design: it only ever clears the orphan's bitmap
    // bit (by design — see `reclaim_orphans`'s and `PHANTOM_DIR_INO`'s own
    // doc comments: it must never look at, let alone alter, content behind
    // a bit it didn't find set), and never zeroes/stamps `i_dtime` on the
    // orphaned inode's own record the way `unlink`/`rmdir` do for a
    // normal deletion (see `CLAUDE.md`'s ext2 section on why that ordering
    // matters there). Real `e2fsck`'s Pass 1 scans the raw inode table
    // directly, not just the bitmap — it still finds the reclaimed
    // inode's well-formed-looking record (nonzero mode, nonzero links,
    // real block pointers) and reports it as a disconnected directory
    // needing reconnection to `lost+found`, `e2fsck -fn` exit code 4, same
    // complaint it would raise before any repair at all. This is not a
    // regression from this migration (the pre-extraction kernel code had
    // the exact same bitmap-only sweep, just never checked against a real
    // `e2fsck` because no host-side test existed to run one) and fixing it
    // is a genuine behavior change to `reclaim_orphans`'s on-disk writes —
    // explicitly out of scope for a "no behavior change" extraction step
    // (see `docs/fs/ext2-extraction-plan.md`'s "Fuera de alcance" section).
    // Flagged here as a real follow-up, not silently dropped.

    /// Build a real, `mke2fs`-created minimal ext2 image (`total_blocks`
    /// 1024-byte blocks, default `mke2fs` geometry otherwise) and return
    /// its raw bytes. `None` if `mke2fs` isn't runnable on this host — the
    /// caller must skip gracefully, not fail, in that case.
    fn build_real_ext2_fixture(total_blocks: u32) -> Option<Vec<u8>> {
        let mut path = std::env::temp_dir();
        path.push(format!("ext2_repair_fixture_{}_{}.img", std::process::id(), total_blocks));
        let size_bytes = total_blocks as u64 * 1024;
        {
            let f = std::fs::File::create(&path).ok()?;
            f.set_len(size_bytes).ok()?;
        }
        // `^resize_inode,^ext_attr`: keep the image as close as possible to
        // this driver's own minimal feature set (see the crate doc
        // comment — only `FILETYPE` is understood), so a passing oracle
        // result says something about *this driver's* repair logic, not
        // about features it doesn't even parse.
        let status = std::process::Command::new("mke2fs")
            .args(["-q", "-F", "-b", "1024", "-I", "128", "-O", "^resize_inode,^ext_attr"])
            .arg(&path)
            .status();
        let ok = matches!(status, Ok(s) if s.success());
        let result = if ok { std::fs::read(&path).ok() } else { None };
        let _ = std::fs::remove_file(&path);
        result
    }

    /// Dump `core`'s current on-disk image to a `Vec<u8>` — reads it back
    /// out through the same `BlockDevice` interface everything else in
    /// this crate uses (`hal::block::MemDisk` doesn't expose its backing
    /// buffer directly).
    fn dump_core_to_bytes(core: &Ext2Core) -> Vec<u8> {
        let total_bytes = core.sb.blocks_count as u64 * core.sb.block_size as u64;
        let sector_size = hal::block::SECTOR_SIZE as u64;
        let total_sectors = total_bytes.div_ceil(sector_size) as u32;
        let mut dump = alloc::vec![0u8; (total_sectors as u64 * sector_size) as usize];
        const CHUNK_SECTORS: u32 = 64; // no real BlockDevice impl needs an unreasonably large single transfer
        let mut lba = 0u32;
        while lba < total_sectors {
            let n = CHUNK_SECTORS.min(total_sectors - lba);
            let start = (lba as u64 * sector_size) as usize;
            let end = start + (n as u64 * sector_size) as usize;
            core.device.read_sectors(lba, n as u8, &mut dump[start..end]).expect("read for e2fsck dump");
            lba += n;
        }
        dump
    }

    /// Run `e2fsck -fn` against `bytes` (dumped to a throwaway temp file).
    /// `Some((clean, stdout, stderr))` if `e2fsck` ran at all, `None` if
    /// it isn't runnable on this host — the caller must skip gracefully,
    /// not fail, on `None`.
    fn e2fsck_says_clean(bytes: &[u8]) -> Option<(bool, String, String)> {
        use std::io::Write;
        let mut path = std::env::temp_dir();
        path.push(format!("ext2_repair_check_{}.img", std::process::id()));
        {
            let mut f = std::fs::File::create(&path).ok()?;
            f.write_all(bytes).ok()?;
        }
        let result = std::process::Command::new("e2fsck").arg("-fn").arg(&path).output();
        let _ = std::fs::remove_file(&path);
        let output = result.ok()?;
        Some((
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }

    /// The real oracle test: a genuine `mke2fs` image, pure free-count
    /// drift injected (no bitmap/content corruption), repaired by
    /// `reconcile_free_counts`, and handed to `e2fsck -fn` for a verdict
    /// instead of a hand-written assertion. See the module comment above
    /// for why this is scoped to `reconcile_free_counts` specifically.
    /// Skips gracefully (an `eprintln!`, not a failure) if `mke2fs`/
    /// `e2fsck` aren't on this host's `$PATH`.
    #[test]
    fn reconcile_free_counts_repairs_drift_on_a_real_mke2fs_image_e2fsck_clean() {
        let Some(bytes) = build_real_ext2_fixture(256) else {
            eprintln!("skipping e2fsck oracle test: mke2fs not runnable on this host");
            return;
        };
        let core = Ext2Core::mount(alloc::boxed::Box::new(MemDisk::from_vec(bytes)))
            .expect("mount a real mke2fs image");

        // Desync free-count bookkeeping only — exactly the "bitmap
        // correct, counters stale" shape an unclean shutdown between a
        // bitmap write and its matching counter update leaves behind (see
        // `reconcile_free_counts`'s own doc comment).
        core.adjust_bgd_counts(0, -3, -1, 0).expect("desync bgd counters");
        core.adjust_sb_counts(-3, -1).expect("desync sb counters");

        let report = core.reconcile_free_counts().expect("reconcile");
        assert!(report.bgd_drift && report.sb_drift, "the injected drift must be detected");

        let dump = dump_core_to_bytes(&core);
        match e2fsck_says_clean(&dump) {
            Some((clean, stdout, stderr)) => assert!(
                clean,
                "e2fsck -fn reported the repaired image as inconsistent:\nstdout: {stdout}\nstderr: {stderr}"
            ),
            None => eprintln!("skipping e2fsck oracle assertion: e2fsck not runnable on this host"),
        }
    }
}
