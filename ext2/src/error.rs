// ext2/src/error.rs
//
// This crate's own error type — see the crate doc comment ("Error type and
// locking") for why it isn't the kernel's `fs::types::Errno`. The kernel
// adapter (`kernel/src/fs/ext2.rs`) implements `From<Ext2Error> for Errno`.

/// Errors this crate's core logic can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ext2Error {
    /// A block device read/write failed, or a block/inode number fell
    /// outside the filesystem's own bounds before ever reaching the
    /// device. Mirrors `Errno::EIO`'s role in the kernel adapter as the
    /// single choke point every on-disk pointer (BGD/inode-table
    /// pointers, block bitmap indices) flows through before being
    /// trusted — see `volume::Ext2Core::read_block`/`write_block`'s doc
    /// comments.
    Io,
    /// `Ext2Core::mount()`: the 2-byte magic at superblock byte offset 56
    /// isn't `0xEF53` — not an ext2 filesystem, or the wrong LBA was read.
    BadMagic,
    /// `Ext2Core::mount()`: `s_feature_incompat` has a bit set beyond
    /// FILETYPE (ext4 extents, a journal, ...). Anything else would
    /// misinterpret `i_block` completely, so mounting refuses outright
    /// rather than guess.
    UnsupportedFeature,
}
