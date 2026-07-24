// kernel/src/hw_tests.rs
//
// QEMU integration test cases — `cargo test --target x86_64-unknown-none`
// (run from `kernel/`), collected via `#[test_case]`
// (`custom_test_frameworks`, see `test_framework.rs`). Only compiled under
// `#[cfg(test)]` (see `mod hw_tests` in `main.rs`).
//
// These assert real hardware-path behavior against a real QEMU boot — the
// `hal/` host tests already cover the pure parsing/decoding logic in
// milliseconds with no QEMU involved; this file is for the part that can't
// be tested that way. `init::test_support::boot_for_tests` (called from
// `kernel_main` before `test_main()` runs these) performs whatever subset
// of the real boot sequence a case here needs already live.

/// Case 1 (Phase 2 of `docs/drivers/roadmap.md`): the ACPI parse against
/// QEMU's real i440fx MADT — Local APIC address, one I/O APIC at the
/// expected base, at least one enabled CPU, and the legacy IRQ0->GSI2
/// PIT/timer override. Previously only a human eyeballing
/// `[acpi] SELFTEST PASS/FAIL` in serial output could catch a regression
/// here; this is the same set of checks (`acpi::selftest_ok`, shared with
/// the boot-time log path) as a real assertion with a real exit code.
#[test_case]
fn acpi_selftest_passes() {
    let topo = crate::acpi::topology()
        .expect("ACPI parse did not populate topology during test boot");
    assert!(
        crate::acpi::selftest_ok(topo),
        "ACPI SELFTEST failed one or more assertions against known QEMU i440fx values"
    );
}

/// Case 2: the storage-stack seam (`hal::block::BlockDevice`, see
/// `hal/src/block.rs`, `kernel/src/block/mod.rs`, and
/// `docs/drivers/architecture.md`'s storage-stack section). Mounts a
/// hand-built minimal ext2 image (`fs::ext2::build_minimal_image` — no
/// `mke2fs`/host-tool dependency, no real disk touched at all) on a
/// `hal::block::MemDisk`, then drives create/write/read/mkdir/rename/
/// symlink/unlink/rmdir through the same real VFS free functions
/// (`fs::vfs::{mkdir,symlink,rename,unlink,rmdir,open,stat}`) every syscall
/// handler goes through. This is the payoff the seam exists for: ext2's
/// read-write path gets a real, repeatable, hardware-free integration test
/// instead of only ever being exercised against the one real `disk.img` at
/// boot — with zero risk of corrupting that image if something goes wrong.
///
/// One big test case, not several, deliberately: `fs::ext2`'s mounted
/// filesystem lives behind a single `spin::Once` global
/// (`fs::ext2::EXT2`), so a second `init_with_device()` call from a
/// separate `#[test_case]` would silently no-op instead of mounting a
/// fresh image — see `fs::ext2::init_with_device`'s doc comment. Scripting
/// the whole scenario in one function avoids that pitfall entirely instead
/// of working around it.
#[test_case]
fn ext2_memdisk_roundtrip() {
    use alloc::boxed::Box;
    use alloc::sync::Arc;
    use crate::block::{BlockDevice, MemDisk};
    use crate::fs::types::OpenFlags;
    use crate::process::file::FileHandle;

    let image = crate::fs::ext2::build_minimal_image();
    let device: Box<dyn BlockDevice> = Box::new(MemDisk::from_vec(image));
    crate::fs::ext2::init_with_device(device)
        .expect("mounting ext2 on a freshly hand-built MemDisk image should succeed");
    crate::fs::vfs::mount("/memtest", Arc::new(crate::fs::ext2::Ext2FsHandle));

    let content: &[u8] = b"hello from a memdisk-backed ext2 mount";
    let write_flags = OpenFlags(OpenFlags::WRONLY.0 | OpenFlags::CREAT.0);

    // create + write
    let mut fh = crate::fs::vfs::open("/memtest/hello.txt", write_flags)
        .expect("create /memtest/hello.txt");
    let n = fh.write(content).expect("write hello.txt");
    assert_eq!(n, content.len(), "write() should report the full content length");
    drop(fh);

    // reopen + read back
    let mut fh = crate::fs::vfs::open("/memtest/hello.txt", OpenFlags::RDONLY)
        .expect("reopen hello.txt for read");
    let mut buf = [0u8; 128];
    let n = fh.read(&mut buf).expect("read hello.txt");
    assert_eq!(&buf[..n], content, "read-back content must match what was written");
    drop(fh);

    // stat reports the real size
    let st = crate::fs::vfs::stat("/memtest/hello.txt").expect("stat hello.txt");
    assert_eq!(st.st_size, content.len() as i64);

    // mkdir + a file nested inside it
    crate::fs::vfs::mkdir("/memtest/subdir").expect("mkdir /memtest/subdir");
    let mut fh = crate::fs::vfs::open("/memtest/subdir/nested.txt", write_flags)
        .expect("create nested.txt inside subdir");
    fh.write(b"nested").expect("write nested.txt");
    drop(fh);

    // symlink + readlink (no-follow) + follow-through stat
    crate::fs::vfs::symlink("hello.txt", "/memtest/hello_link").expect("symlink hello_link -> hello.txt");
    let link_target = crate::fs::vfs::resolve_no_follow("/memtest/hello_link")
        .expect("resolve the symlink itself, not its target")
        .readlink()
        .expect("readlink hello_link");
    assert_eq!(link_target, "hello.txt");
    let st = crate::fs::vfs::stat("/memtest/hello_link").expect("stat follows the symlink to hello.txt");
    assert_eq!(st.st_size, content.len() as i64);

    // rename (within the same directory)
    crate::fs::vfs::rename("/memtest/subdir/nested.txt", "/memtest/subdir/renamed.txt")
        .expect("rename nested.txt -> renamed.txt");
    assert!(crate::fs::vfs::resolve("/memtest/subdir/nested.txt").is_err(), "old name must be gone after rename");
    assert!(crate::fs::vfs::resolve("/memtest/subdir/renamed.txt").is_ok(), "new name must resolve after rename");

    // unlink + rmdir cleanup, verifying each removal actually took
    crate::fs::vfs::unlink("/memtest/subdir/renamed.txt").expect("unlink renamed.txt");
    crate::fs::vfs::unlink("/memtest/hello_link").expect("unlink hello_link");
    crate::fs::vfs::rmdir("/memtest/subdir").expect("rmdir now-empty subdir");
    crate::fs::vfs::unlink("/memtest/hello.txt").expect("unlink hello.txt");

    assert!(crate::fs::vfs::resolve("/memtest/hello.txt").is_err());
    assert!(crate::fs::vfs::resolve("/memtest/hello_link").is_err());
    assert!(crate::fs::vfs::resolve("/memtest/subdir").is_err());
}

/// Case 3: diagnostic for a real `e2fsck -fn disk.img` report against the
/// *real* boot disk —
///
/// ```text
/// El directorio del nodo-i 31 está desconectado (estaba en /)
/// '..' en ... (31) es / (2) y debería ser <El nodo-i NULO> (0)
/// La cuenta de referencia del nodo-i 2 es 6, y debería ser 7
/// La cuenta de referencia del nodo-i 31 es 2, y debería ser 1
/// Diferencias del mapa de bits del bloque:  +56702
/// Diferencias del mapa de bits del nodo-i:  +31
/// ```
///
/// This test builds TWO different hand-crafted shapes on a `MemDisk` and
/// checks how `reclaim_orphans` treats each — see `fs::ext2::
/// build_image_with_orphans`'s doc comment for the byte-level layout of
/// both:
///
///   1. `ORPHAN_FILE_INO`/`ORPHAN_DIR_INO` (the latter using inode number
///      31 deliberately, to mirror the real report): an inode + block
///      *marked used* in the bitmaps with nothing reachable from root —
///      the shape `reclaim_orphans`'s own doc comment says it exists to
///      sweep.
///   2. `PHANTOM_DIR_INO`/`PHANTOM_DIR_BLOCK`: same disconnected directory
///      shape (real inode record, real "."/".."->root data block,
///      nothing under root pointing at it) but with **both bitmap bits
///      left clear** instead of set.
///
/// Shape 2, not shape 1, is what a real disk-side reproduction of the
/// actual suspect showed. `sync_disk_bin_dir()`'s debugfs script runs
/// `mkdir /bin` unconditionally on *every* build, including every rebuild
/// after the first (`disk.img` is create-once — see `ensure_ext2_disk_
/// image`'s doc comment — so `/bin` already exists on every build after
/// the first one that created it). Reproduced by hand on a throwaway
/// image (never `disk.img`, `e2fsck -fn` only): a real `debugfs -w -R
/// "mkdir /bin"` against a `/bin` that already exists fails with
/// `ext2fs_mkdir2: Ext2 directory already exists` — but only *after*
/// libext2fs has already allocated a fresh inode, written its "."/".."
/// (parent=root) directory block, and written the inode record itself;
/// the failed final link-into-parent step leaves all of that behind
/// without ever marking either bitmap bit. `debugfs -R "testi <N>"`/
/// `"testb <N>"` against the resulting image confirmed the leaked
/// inode/block are reported "not in use" — i.e. free per the bitmap, with
/// live, non-zeroed content sitting behind that "free" claim — and the
/// resulting `e2fsck -fn` report matched the real `disk.img` report
/// wording and link-count-delta direction exactly (down to `'..'`
/// pointing at root and the same +1/-1 shape on inode 2's and the
/// orphan's own link counts). This is a real, independently-reproducible
/// bug in `debugfs`/`libext2fs`'s `mkdir` error path (present at least in
/// `e2fsprogs` 1.47.4), not something to fix in this kernel — but
/// `sync_disk_bin_dir()` running its `mkdir /bin` unconditionally on every
/// build, instead of only when `/bin` doesn't already exist, is what
/// actually triggers it against `disk.img`, repeatedly, across this
/// project's history.
///
/// Given that, shape 2 is also *why* `disk.img`'s mtime didn't change
/// across a boot that mounted `/mnt`: `reclaim_orphans`'s sweep loop only
/// ever inspects and *clears* a bit that starts out **set** (`if
/// block_bitmap[byte] & mask == 0 { continue; }`/ same for the inode
/// bitmap) — a bit that's already clear is invisible to it by
/// construction, regardless of what stale inode-table/data-block content
/// sits behind it. So a mount finding shape-2 corruption correctly has
/// nothing to write back. This isn't a gap in the walk's reachability
/// logic (shape 1 below proves the walk itself is correct); it's a
/// corruption shape genuinely outside `reclaim_orphans`'s stated contract
/// ("frees any block/inode the bitmaps mark used that the walk never
/// reached" — shape 2 isn't marked used to begin with).
///
/// Uses `fs::ext2::TestFs` (mounts a private `Ext2Fs`, bypassing the
/// `EXT2` global `Once`) rather than `init_with_device`/the VFS —
/// `ext2_memdisk_roundtrip` above already claimed the global for this
/// boot, and a second `init_with_device()` call would silently no-op
/// instead of mounting this fresh image (see that function's doc
/// comment). This also means this test needs no VFS mount at all: it only
/// cares about on-disk bitmap/counter state before and after
/// `reclaim_orphans`, not filesystem operations through it.
#[test_case]
fn ext2_reclaim_orphans_clears_injected_disk_img_shape() {
    use alloc::boxed::Box;
    use crate::block::{BlockDevice, MemDisk};
    use crate::fs::ext2::{
        self, ORPHAN_DIR_BLOCK, ORPHAN_DIR_INO, ORPHAN_FILE_BLOCK, ORPHAN_FILE_INO,
        PHANTOM_DIR_BLOCK, PHANTOM_DIR_INO,
    };

    let image = ext2::build_image_with_orphans();
    let device: Box<dyn BlockDevice> = Box::new(MemDisk::from_vec(image));
    let fs = ext2::TestFs::mount(device)
        .expect("mounting the hand-built orphan image should succeed");

    // Sanity: the image really does start with both orphans marked used —
    // if this fails, the image builder itself doesn't reproduce the bug
    // shape and the rest of this test is meaningless.
    assert!(fs.inode_used(ORPHAN_FILE_INO).unwrap(), "orphan file inode must start marked used");
    assert!(fs.block_used(ORPHAN_FILE_BLOCK).unwrap(), "orphan file block must start marked used");
    assert!(fs.inode_used(ORPHAN_DIR_INO).unwrap(), "orphan dir inode (31) must start marked used");
    assert!(fs.block_used(ORPHAN_DIR_BLOCK).unwrap(), "orphan dir block must start marked used");

    // Sanity for the phantom shape: bitmap bits already clear (free) even
    // though real directory content sits behind them.
    assert!(!fs.inode_used(PHANTOM_DIR_INO).unwrap(), "phantom dir inode must start marked FREE despite real content");
    assert!(!fs.block_used(PHANTOM_DIR_BLOCK).unwrap(), "phantom dir block must start marked FREE despite real content");
    let phantom_mode_before = fs.inode_mode(PHANTOM_DIR_INO).unwrap();
    assert_eq!(phantom_mode_before, 0x4000 | 0o755, "phantom inode record must start with real directory content");

    // The image is built with free-count fields already consistent with
    // the (orphan-including) bitmaps, so this should be a no-op — isolates
    // what's under test to reclaim_orphans, not reconcile_free_counts.
    fs.reconcile_free_counts().expect("reconcile_free_counts should succeed against a consistent image");
    let (sb_free_blocks_before, sb_free_inodes_before) = fs.sb_free_counts().unwrap();
    let (true_free_blocks_before, true_free_inodes_before) = fs.true_free_counts_group0().unwrap();
    assert_eq!(
        sb_free_blocks_before, true_free_blocks_before as u32,
        "reconcile_free_counts should have left the superblock's free-block count matching the bitmap"
    );
    assert_eq!(
        sb_free_inodes_before, true_free_inodes_before as u32,
        "reconcile_free_counts should have left the superblock's free-inode count matching the bitmap"
    );

    // This is the real question: does the mount-time orphan sweep clear
    // an inode 31-shaped orphan (a disconnected directory whose ".."
    // points at root) the same way it clears a plain orphan file?
    fs.reclaim_orphans().expect("reclaim_orphans should complete without an I/O error against this image");

    assert!(!fs.inode_used(ORPHAN_FILE_INO).unwrap(), "reclaim_orphans should have freed the orphan file inode");
    assert!(!fs.block_used(ORPHAN_FILE_BLOCK).unwrap(), "reclaim_orphans should have freed the orphan file block");
    assert!(!fs.inode_used(ORPHAN_DIR_INO).unwrap(), "reclaim_orphans should have freed the orphan dir inode (31)");
    assert!(!fs.block_used(ORPHAN_DIR_BLOCK).unwrap(), "reclaim_orphans should have freed the orphan dir block");

    // Root itself, and its own data block, must NOT have been swept —
    // reclaim_orphans clearing everything (including root) would trivially
    // "pass" the four assertions above for the wrong reason. This is the
    // regression the CLAUDE.md-documented walk-order bug produced: root
    // pre-marked reserved before the reachability walk ran made the very
    // first `mark_reachable` call a no-op, so the sweep freed almost
    // everything, root included.
    assert!(fs.inode_used(2).unwrap(), "root's own inode must still be marked used after reclaim");
    assert!(fs.block_used(21).unwrap(), "root's own directory data block must still be marked used after reclaim");

    // Free counters must reflect the 2 reclaimed inodes / 2 reclaimed
    // blocks, and stay self-consistent with the bitmaps they summarize —
    // reclaim_orphans re-runs reconcile_free_counts internally when it
    // changes anything, so this checks that path too, not just the sweep.
    let (sb_free_blocks_after, sb_free_inodes_after) = fs.sb_free_counts().unwrap();
    let (bgd_free_blocks_after, bgd_free_inodes_after) = fs.bgd_free_counts(0).unwrap();
    let (true_free_blocks_after, true_free_inodes_after) = fs.true_free_counts_group0().unwrap();

    assert_eq!(sb_free_blocks_after, sb_free_blocks_before + 2, "2 blocks should have been reclaimed");
    assert_eq!(sb_free_inodes_after, sb_free_inodes_before + 2, "2 inodes should have been reclaimed");
    assert_eq!(sb_free_blocks_after, true_free_blocks_after as u32, "superblock free-block count must match the bitmap post-reclaim");
    assert_eq!(sb_free_inodes_after, true_free_inodes_after as u32, "superblock free-inode count must match the bitmap post-reclaim");
    assert_eq!(bgd_free_blocks_after, true_free_blocks_after, "BGD free-block count must match the bitmap post-reclaim");
    assert_eq!(bgd_free_inodes_after, true_free_inodes_after, "BGD free-inode count must match the bitmap post-reclaim");

    // The phantom shape (bitmap bits already clear, real content behind
    // them) must survive `reclaim_orphans` completely unchanged — this is
    // the documented scope limit, not a bug: the sweep never looks at a
    // bit that starts clear, so it can neither notice nor disturb this
    // shape. If either assertion below ever fails, `reclaim_orphans`
    // changed behavior in a way that would need re-auditing against this
    // diagnosis.
    assert!(!fs.inode_used(PHANTOM_DIR_INO).unwrap(), "phantom dir inode must still read as free after reclaim (out of scope for the sweep)");
    assert!(!fs.block_used(PHANTOM_DIR_BLOCK).unwrap(), "phantom dir block must still read as free after reclaim (out of scope for the sweep)");
    assert_eq!(
        fs.inode_mode(PHANTOM_DIR_INO).unwrap(), phantom_mode_before,
        "phantom inode's real content must be completely untouched by reclaim_orphans — it never reads a bit it didn't find set"
    );
}
