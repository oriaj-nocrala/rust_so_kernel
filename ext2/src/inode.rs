// ext2/src/inode.rs
//
// The raw on-disk inode record. Moved verbatim out of
// `kernel::fs::ext2::RawInode` — migration step 1, no behavior change.
// Every method here is pure byte manipulation; nothing in this file
// touches a `BlockDevice`. Locating/reading/writing an inode record on an
// actual device (`inode_location`/`read_inode`/`write_inode`) stays in
// `kernel::fs::ext2` for now — it isn't needed by block/inode allocation
// (migration step 2 doesn't touch the inode table at all, only the inode
// *bitmap*), and moving it isn't required to keep this crate self-
// contained for what it does cover.

use alloc::vec::Vec;

/// A raw on-disk inode record, kept as its exact `inode_size` bytes rather
/// than a handful of decoded fields — so a write-back only patches the
/// fields this driver actually understands/manages (mode, size, links
/// count, block count, block pointers) and leaves everything else (times,
/// uid/gid, ACL/generation fields) exactly as read, instead of silently
/// zeroing them.
#[derive(Clone)]
pub struct RawInode {
    pub buf: Vec<u8>,
}

impl RawInode {
    pub fn parse(buf: &[u8]) -> Self {
        Self { buf: buf.to_vec() }
    }

    /// A brand-new, all-zero inode record of `size` bytes (`inode_size`) —
    /// used by `create`/`mkdir` before filling in mode/links/blocks.
    pub fn zeroed(size: usize) -> Self {
        Self { buf: alloc::vec![0u8; size] }
    }

    pub fn i_mode(&self) -> u16 {
        u16::from_le_bytes(self.buf[0..2].try_into().unwrap())
    }

    pub fn set_i_mode(&mut self, v: u16) {
        self.buf[0..2].copy_from_slice(&v.to_le_bytes());
    }

    pub fn links_count(&self) -> u16 {
        u16::from_le_bytes(self.buf[26..28].try_into().unwrap())
    }

    pub fn set_links_count(&mut self, v: u16) {
        self.buf[26..28].copy_from_slice(&v.to_le_bytes());
    }

    pub fn set_blocks_512(&mut self, v: u32) {
        self.buf[28..32].copy_from_slice(&v.to_le_bytes());
    }

    /// `i_dtime` (deletion time, real Unix epoch seconds) — real ext2
    /// stamps this when an inode's last link is removed. Earlier this
    /// wrote a raw boot-relative uptime value here instead (this kernel
    /// had no RTC at the time), which is small enough (single/double-digit
    /// seconds) to collide with a *different* on-disk use of this same
    /// field: ext3+ threads its in-progress orphan-inode list through
    /// `i_dtime` as a next-inode-number link while a deletion is
    /// mid-flight across a crash, and `e2fsck` tells the two uses apart
    /// purely by plausibility (a real calendar time is ~10 digits; a small
    /// value reads as a link, not a timestamp) — so that raw uptime got
    /// misdiagnosed as "part of a corrupted orphan inode list" and
    /// silently rewritten. Now backed by a real CMOS RTC reading (see
    /// `kernel::rtc`/`kernel::time::now_unix_secs`, still called by the
    /// kernel adapter, not this crate — see the crate doc comment on
    /// clocks staying out of scope until a step that actually needs one),
    /// so this is a genuine wall-clock timestamp and the collision doesn't
    /// come up.
    pub fn set_dtime(&mut self, unix_secs: u32) {
        self.buf[20..24].copy_from_slice(&unix_secs.to_le_bytes());
    }

    /// `size_hi` (`i_dir_acl`/`i_size_high`) only means "upper size bits"
    /// for regular files under the large_file feature; for directories
    /// it's genuinely the (unused, by us) ACL block pointer, so it's only
    /// read/written when this inode is a regular file.
    pub fn size(&self) -> u64 {
        let size_lo = u32::from_le_bytes(self.buf[4..8].try_into().unwrap());
        if self.is_reg() {
            let size_hi = u32::from_le_bytes(self.buf[108..112].try_into().unwrap());
            ((size_hi as u64) << 32) | size_lo as u64
        } else {
            size_lo as u64
        }
    }

    pub fn set_size(&mut self, v: u64) {
        self.buf[4..8].copy_from_slice(&((v & 0xFFFF_FFFF) as u32).to_le_bytes());
        if self.is_reg() {
            self.buf[108..112].copy_from_slice(&((v >> 32) as u32).to_le_bytes());
        }
    }

    pub fn i_block(&self, i: usize) -> u32 {
        let off = 40 + i * 4;
        u32::from_le_bytes(self.buf[off..off + 4].try_into().unwrap())
    }

    pub fn set_i_block(&mut self, i: usize, v: u32) {
        let off = 40 + i * 4;
        self.buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    pub fn is_dir(&self) -> bool {
        (self.i_mode() & 0xF000) == 0x4000
    }

    pub fn is_reg(&self) -> bool {
        (self.i_mode() & 0xF000) == 0x8000
    }

    pub fn is_symlink(&self) -> bool {
        (self.i_mode() & 0xF000) == 0xA000
    }

    /// A symlink short enough that its target lives inline in `i_block`'s
    /// own bytes instead of a real data block — see the kernel adapter's
    /// module doc comment for the full "fast" vs "slow" symlink
    /// explanation.
    pub fn is_fast_symlink(&self) -> bool {
        self.is_symlink() && self.size() < 60
    }

    /// True if `i_block`'s 15 slots hold real block-number pointers safe
    /// to walk — false for a fast symlink (inline text, not pointers) and
    /// for any inode type this driver never creates itself (char/block
    /// device, FIFO, socket), whose `i_block` encoding means something
    /// else entirely (e.g. a device's major/minor pair) that would
    /// misread as wild pointers if walked the same way.
    pub fn has_block_pointers(&self) -> bool {
        (self.is_dir() || self.is_reg() || self.is_symlink()) && !self.is_fast_symlink()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zeroed_has_expected_length_and_all_zero_bytes() {
        let raw = RawInode::zeroed(128);
        assert_eq!(raw.buf.len(), 128);
        assert!(raw.buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn parse_copies_bytes_verbatim() {
        let bytes: alloc::vec::Vec<u8> = (0..128u32).map(|i| i as u8).collect();
        let raw = RawInode::parse(&bytes);
        assert_eq!(raw.buf, bytes);
    }

    #[test]
    fn mode_round_trips() {
        let mut raw = RawInode::zeroed(128);
        raw.set_i_mode(0x8000 | 0o644);
        assert_eq!(raw.i_mode(), 0x8000 | 0o644);
        assert!(raw.is_reg());
        assert!(!raw.is_dir());
        assert!(!raw.is_symlink());
    }

    #[test]
    fn dir_mode_is_dir() {
        let mut raw = RawInode::zeroed(128);
        raw.set_i_mode(0x4000 | 0o755);
        assert!(raw.is_dir());
    }

    #[test]
    fn links_count_round_trips() {
        let mut raw = RawInode::zeroed(128);
        raw.set_links_count(3);
        assert_eq!(raw.links_count(), 3);
    }

    #[test]
    fn regular_file_size_round_trips_including_high_bits() {
        let mut raw = RawInode::zeroed(128);
        raw.set_i_mode(0x8000 | 0o644);
        let big = (1u64 << 33) + 42; // needs size_hi
        raw.set_size(big);
        assert_eq!(raw.size(), big);
    }

    #[test]
    fn directory_size_ignores_size_hi_bytes() {
        let mut raw = RawInode::zeroed(128);
        raw.set_i_mode(0x4000 | 0o755);
        // Poison what would be size_hi for a regular file — must not be
        // read back for a directory.
        raw.buf[108..112].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        raw.set_size(4096);
        assert_eq!(raw.size(), 4096);
    }

    #[test]
    fn i_block_round_trips_all_15_slots() {
        let mut raw = RawInode::zeroed(128);
        for i in 0..15 {
            raw.set_i_block(i, 1000 + i as u32);
        }
        for i in 0..15 {
            assert_eq!(raw.i_block(i), 1000 + i as u32);
        }
    }

    #[test]
    fn fast_symlink_detection() {
        let mut raw = RawInode::zeroed(128);
        raw.set_i_mode(0xA000 | 0o777);
        raw.buf[40..40 + 4].copy_from_slice(b"/bin");
        raw.set_size(4);
        assert!(raw.is_fast_symlink());
        assert!(!raw.has_block_pointers());
    }

    #[test]
    fn slow_symlink_is_not_fast_and_has_block_pointers() {
        let mut raw = RawInode::zeroed(128);
        raw.set_i_mode(0xA000 | 0o777);
        raw.set_size(100); // >= 60 -> slow representation
        assert!(!raw.is_fast_symlink());
        assert!(raw.has_block_pointers());
    }

    #[test]
    fn regular_and_directory_have_block_pointers() {
        let mut reg = RawInode::zeroed(128);
        reg.set_i_mode(0x8000 | 0o644);
        assert!(reg.has_block_pointers());

        let mut dir = RawInode::zeroed(128);
        dir.set_i_mode(0x4000 | 0o755);
        assert!(dir.has_block_pointers());
    }

    #[test]
    fn dtime_round_trips() {
        let mut raw = RawInode::zeroed(128);
        raw.set_dtime(1_700_000_000);
        let v = u32::from_le_bytes(raw.buf[20..24].try_into().unwrap());
        assert_eq!(v, 1_700_000_000);
    }

    #[test]
    fn blocks_512_round_trips() {
        let mut raw = RawInode::zeroed(128);
        raw.set_blocks_512(8);
        let v = u32::from_le_bytes(raw.buf[28..32].try_into().unwrap());
        assert_eq!(v, 8);
    }
}
