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
/// hand-built minimal ext2 image (`ext2::testimg::build_minimal_image` — no
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

    let image = ext2::testimg::build_minimal_image();
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
/// Mounts a standalone `ext2::Ext2Core` directly (bypassing the kernel's
/// `EXT2` global `Once` and the VFS entirely) rather than
/// `init_with_device`/the VFS — `ext2_memdisk_roundtrip` above already
/// claimed the global for this boot, and a second `init_with_device()`
/// call would silently no-op instead of mounting this fresh image (see
/// that function's doc comment). This also means this test needs no VFS
/// mount at all: it only cares about on-disk bitmap/counter state before
/// and after `reclaim_orphans`, not filesystem operations through it. Prior
/// to the ext2-extraction-plan step 6 cleanup this went through a
/// kernel-local `fs::ext2::TestFs` wrapper around a private `Ext2Fs`; that
/// wrapper's methods were all thin pass-throughs to `ext2::Ext2Core` (the
/// same type this test mounts now), so removing it changes nothing about
/// what's being exercised.
#[test_case]
fn ext2_reclaim_orphans_clears_injected_disk_img_shape() {
    use alloc::boxed::Box;
    use crate::block::{BlockDevice, MemDisk};
    use ext2::testimg::{
        build_image_with_orphans, ORPHAN_DIR_BLOCK, ORPHAN_DIR_INO, ORPHAN_FILE_BLOCK,
        ORPHAN_FILE_INO, PHANTOM_DIR_BLOCK, PHANTOM_DIR_INO,
    };

    let image = build_image_with_orphans();
    let device: Box<dyn BlockDevice> = Box::new(MemDisk::from_vec(image));
    let core = ext2::Ext2Core::mount(device)
        .expect("mounting the hand-built orphan image should succeed");

    // Sanity: the image really does start with both orphans marked used —
    // if this fails, the image builder itself doesn't reproduce the bug
    // shape and the rest of this test is meaningless.
    assert!(core.inode_used(ORPHAN_FILE_INO).unwrap(), "orphan file inode must start marked used");
    assert!(core.block_used(ORPHAN_FILE_BLOCK).unwrap(), "orphan file block must start marked used");
    assert!(core.inode_used(ORPHAN_DIR_INO).unwrap(), "orphan dir inode (31) must start marked used");
    assert!(core.block_used(ORPHAN_DIR_BLOCK).unwrap(), "orphan dir block must start marked used");

    // Sanity for the phantom shape: bitmap bits already clear (free) even
    // though real directory content sits behind them.
    assert!(!core.inode_used(PHANTOM_DIR_INO).unwrap(), "phantom dir inode must start marked FREE despite real content");
    assert!(!core.block_used(PHANTOM_DIR_BLOCK).unwrap(), "phantom dir block must start marked FREE despite real content");
    let phantom_mode_before = core.inode_mode(PHANTOM_DIR_INO).unwrap();
    assert_eq!(phantom_mode_before, 0x4000 | 0o755, "phantom inode record must start with real directory content");

    // The image is built with free-count fields already consistent with
    // the (orphan-including) bitmaps, so this should be a no-op — isolates
    // what's under test to reclaim_orphans, not reconcile_free_counts.
    let _ = core.reconcile_free_counts().expect("reconcile_free_counts should succeed against a consistent image");
    let (sb_free_blocks_before, sb_free_inodes_before) = core.sb_free_counts().unwrap();
    let (true_free_blocks_before, true_free_inodes_before) = core.true_free_counts_group0().unwrap();
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
    // A real wall clock is available here (unlike the `ext2` crate's own
    // host tests) — this is the same value the kernel adapter passes at a
    // real mount, so the dtime assertions below exercise the production
    // path, not a test-only constant.
    let dtime = crate::time::now_unix_secs() as u32;
    let _ = core.reclaim_orphans(dtime).expect("reclaim_orphans should complete without an I/O error against this image");

    assert!(!core.inode_used(ORPHAN_FILE_INO).unwrap(), "reclaim_orphans should have freed the orphan file inode");
    assert!(!core.block_used(ORPHAN_FILE_BLOCK).unwrap(), "reclaim_orphans should have freed the orphan file block");
    assert!(!core.inode_used(ORPHAN_DIR_INO).unwrap(), "reclaim_orphans should have freed the orphan dir inode (31)");
    assert!(!core.block_used(ORPHAN_DIR_BLOCK).unwrap(), "reclaim_orphans should have freed the orphan dir block");

    // Clearing the bitmap bit is only half the job: real `e2fsck`'s Pass 1
    // scans the raw inode table, so a reclaimed inode whose record still
    // looks live gets reported as a disconnected inode needing
    // `lost+found` — see `ext2::repair`'s e2fsck-oracle tests, which catch
    // that against a real `mke2fs`/`debugfs` image. This asserts the same
    // property on the real hardware path.
    assert_eq!(core.inode_mode(ORPHAN_FILE_INO).unwrap(), 0, "reclaimed orphan file's inode record must be zeroed, not just its bitmap bit");
    assert_eq!(core.inode_mode(ORPHAN_DIR_INO).unwrap(), 0, "reclaimed orphan dir's inode record must be zeroed, not just its bitmap bit");

    // Root itself, and its own data block, must NOT have been swept —
    // reclaim_orphans clearing everything (including root) would trivially
    // "pass" the four assertions above for the wrong reason. This is the
    // regression the CLAUDE.md-documented walk-order bug produced: root
    // pre-marked reserved before the reachability walk ran made the very
    // first `mark_reachable` call a no-op, so the sweep freed almost
    // everything, root included.
    assert!(core.inode_used(2).unwrap(), "root's own inode must still be marked used after reclaim");
    assert!(core.block_used(21).unwrap(), "root's own directory data block must still be marked used after reclaim");

    // Free counters must reflect the 2 reclaimed inodes / 2 reclaimed
    // blocks, and stay self-consistent with the bitmaps they summarize —
    // reclaim_orphans re-runs reconcile_free_counts internally when it
    // changes anything, so this checks that path too, not just the sweep.
    let (sb_free_blocks_after, sb_free_inodes_after) = core.sb_free_counts().unwrap();
    let (bgd_free_blocks_after, bgd_free_inodes_after) = core.bgd_free_counts(0).unwrap();
    let (true_free_blocks_after, true_free_inodes_after) = core.true_free_counts_group0().unwrap();

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
    assert!(!core.inode_used(PHANTOM_DIR_INO).unwrap(), "phantom dir inode must still read as free after reclaim (out of scope for the sweep)");
    assert!(!core.block_used(PHANTOM_DIR_BLOCK).unwrap(), "phantom dir block must still read as free after reclaim (out of scope for the sweep)");
    assert_eq!(
        core.inode_mode(PHANTOM_DIR_INO).unwrap(), phantom_mode_before,
        "phantom inode's real content must be completely untouched by reclaim_orphans — it never reads a bit it didn't find set"
    );
}

/// Case 4: the AF_UNIX **adapter** (`kernel/src/ipc/unix.rs`) — the half of
/// the socket stack the `usock` crate's 71 host tests cannot reach.
///
/// `usock` proves the state machines in plain types. What only a real boot
/// can prove is the wiring around them: that the global table really is an
/// `IrqMutex` a kernel path can take and release, that `UnixSocketHandle`
/// behaves as a `FileHandle` (`read`/`write`/`dup`, and reference counting
/// in `Drop` rather than in `close()`), and that `socket_id()` — the seam
/// that replaced the old pid-indexed `FD_CHANNEL_MAP` — reports through a
/// `Box<dyn FileHandle>`, where no downcast is possible.
///
/// Deliberately does not go through the syscall layer: there is no user
/// process here to make a syscall, and the blocking paths end in
/// `jump_to_user`, which would never come back to the test harness. The
/// syscall layer is covered end-to-end instead by `userspace/c/socket_test.c`
/// (46 checks through real mlibc), and the blocking paths by `ipc_ping`.
#[test_case]
fn unix_socket_handle_roundtrip() {
    use alloc::boxed::Box;
    use crate::ipc::unix::{UnixSocketHandle, SOCKETS};
    use crate::process::file::{FileError, FileHandle};
    use usock::SockType;

    let (a, b) = SOCKETS
        .with(|t| t.socketpair(SockType::Stream))
        .expect("socketpair should succeed on a freshly booted kernel");

    let mut left: Box<dyn FileHandle> = Box::new(UnixSocketHandle::new(a));
    let mut right: Box<dyn FileHandle> = Box::new(UnixSocketHandle::new(b));

    // The seam that replaced the pid-indexed side table: a socket behind a
    // trait object can still name itself.
    assert_eq!(left.socket_id(), Some(a), "socket_id() must survive the Box<dyn FileHandle>");
    assert_eq!(right.socket_id(), Some(b));

    assert_eq!(left.write(b"over the seam").expect("write"), 13);
    let mut buf = [0u8; 32];
    let n = right.read(&mut buf).expect("read");
    assert_eq!(&buf[..n], b"over the seam", "bytes must cross the adapter intact");

    // Nothing queued: a read reports WouldBlock rather than a short read,
    // which is what tells `sys_read` to park the process.
    assert!(
        matches!(right.read(&mut buf), Err(FileError::WouldBlock)),
        "an empty connected socket must report WouldBlock, not EOF"
    );

    // dup() shares the socket: closing one reference must not disconnect it.
    let dup = right.dup().expect("a socket handle is dup-able (fork/dup2 depend on it)");
    drop(right);
    assert_eq!(left.write(b"still here").expect("write after partial close"), 10);
    drop(dup);

    // With the last reference gone, the peer sees a dead connection.
    assert!(
        matches!(left.write(b"x"), Err(FileError::BrokenPipe)),
        "writing to a fully closed peer must be EPIPE"
    );
    let n = left.read(&mut buf).expect("reading a dead peer is EOF, not an error");
    assert_eq!(n, 0);

    drop(left);
    assert!(
        SOCKETS.with(|t| t.get(a).is_none() && t.get(b).is_none()),
        "both sockets must be released once every handle is dropped"
    );
}

/// Case 5: the framebuffer's drawing primitives, against a RAM-backed
/// `Framebuffer` — the same technique `ext2_memdisk_roundtrip` uses with
/// `MemDisk`, applied to the one other driver whose output is a byte
/// buffer somebody can read back and assert on.
///
/// It exists because `fill_rect` and `draw_char`'s fast path replaced
/// straightforward per-pixel loops with span-at-a-time writes composed out
/// of a pattern buffer (see `Framebuffer::fill_rect`), and the failure
/// mode of getting that arithmetic wrong is invisible in the place it
/// matters: on the target machine the screen is the only output there is,
/// a `stride > width` framebuffer smears every row by a few pixels, and
/// finding that out costs a `dd` to a pendrive and a reboot. `stride` is
/// deliberately larger than `width` here — the padding columns are exactly
/// what a naive `row * width` would corrupt — and the buffer starts filled
/// with `0xAA` so "untouched" is a thing the test can actually assert,
/// rather than being indistinguishable from "written with zeros".
#[test_case]
fn framebuffer_primitives_touch_exactly_their_own_pixels() {
    use alloc::boxed::Box;
    use alloc::vec;
    use crate::framebuffer::{Color, Framebuffer, GLYPH_H, GLYPH_W};

    const W: usize = 32;
    const H: usize = 16;
    const STRIDE: usize = 40; // deliberately > W
    const BPP: usize = 4;

    let buf: &'static mut [u8] = Box::leak(vec![0xAAu8; H * STRIDE * BPP].into_boxed_slice());
    let base = buf.as_ptr() as usize;
    let mut fb = Framebuffer::new(buf, W, H, STRIDE, BPP);

    // Read a pixel back out of the same memory the framebuffer writes to.
    let px = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * STRIDE + x) * BPP;
        // SAFETY: `base` is the leaked buffer, alive for the rest of the
        // boot, and `off + 4` is inside `H * STRIDE * BPP` for every
        // coordinate used below.
        let p = (base + off) as *const u8;
        unsafe { [*p, *p.add(1), *p.add(2), *p.add(3)] }
    };

    // ── A coloured rectangle: the pattern path ───────────────────────
    let c = Color::rgb(0x11, 0x22, 0x33);
    fb.fill_rect(2, 3, 5, 4, c);
    assert_eq!(px(2, 3), [0x33, 0x22, 0x11, 0x00], "pixels are written B,G,R");
    assert_eq!(px(6, 6), [0x33, 0x22, 0x11, 0x00], "bottom-right corner is inclusive of w-1/h-1");
    assert_eq!(px(7, 6), [0xAA; 4], "one column past the rectangle must be untouched");
    assert_eq!(px(2, 7), [0xAA; 4], "one row past the rectangle must be untouched");
    assert_eq!(px(1, 3), [0xAA; 4], "one column before the rectangle must be untouched");

    // The padding columns between `width` and `stride` are what a
    // `row * width` slip would silently eat.
    for y in 0..H {
        for x in W..STRIDE {
            assert_eq!(px(x, y), [0xAA; 4], "stride padding at ({x},{y}) must never be written");
        }
    }

    // ── Black: the memset path, which also clears the 4th byte ───────
    fb.fill_rect(2, 3, 5, 4, Color::rgb(0, 0, 0));
    assert_eq!(px(3, 4), [0, 0, 0, 0], "a black fill zeroes the whole pixel");
    assert_eq!(px(7, 4), [0xAA; 4], "the black path respects the same bounds");

    // ── Clamping: a rectangle running off every edge ─────────────────
    // `ESC[J` on the bottom row passes exactly this shape. It must clip,
    // not panic and not write past the last scanline.
    fb.fill_rect(W - 2, H - 2, 999, 999, Color::rgb(0, 0, 0));
    assert_eq!(px(W - 1, H - 1), [0, 0, 0, 0]);
    assert_eq!(px(W, H - 1), [0xAA; 4], "clipping stops at `width`, not `stride`");
    fb.fill_rect(W + 10, H + 10, 4, 4, c); // entirely off-screen: a no-op

    // ── A glyph: fast path (cell fully on screen) ────────────────────
    let fg = Color::rgb(0xFF, 0xFF, 0xFF);
    let bg = Color::rgb(0x01, 0x02, 0x03);
    fb.draw_char(8, 8, b'A', fg, bg, 1);

    let glyph = font8x8::legacy::BASIC_LEGACY[b'A' as usize];
    let mut lit = 0usize;
    for row in 0..GLYPH_H {
        for col in 0..GLYPH_W {
            let set = (glyph[row] >> col) & 1 != 0;
            let got = px(8 + col, 8 + row);
            if set {
                lit += 1;
                assert_eq!(got, [0xFF, 0xFF, 0xFF, 0x00], "lit pixel at ({col},{row}) of 'A'");
            } else {
                assert_eq!(got, [0x03, 0x02, 0x01, 0x00], "background pixel at ({col},{row}) of 'A'");
            }
        }
    }
    assert!(lit > 0, "the glyph for 'A' must have some lit pixels — wrong font indexing otherwise");
    assert_eq!(px(8 + GLYPH_W, 8), [0xAA; 4], "a glyph must not bleed into the next cell");

    // ── A glyph half off the right edge: slow, clipped path ──────────
    fb.draw_char(W - 3, 0, b'B', fg, bg, 1);
    assert_ne!(px(W - 3, 0), [0xAA; 4], "the on-screen part of a clipped glyph is drawn");
    for y in 0..GLYPH_H {
        assert_eq!(px(W, y), [0xAA; 4], "a clipped glyph must not spill into stride padding");
    }

    // ── scroll_up ────────────────────────────────────────────────────
    fb.fill_rect(0, 0, W, H, Color::rgb(0, 0, 0));
    fb.fill_rect(0, 4, W, 1, c); // one marker scanline
    fb.scroll_up(4);
    assert_eq!(px(0, 0), [0x33, 0x22, 0x11, 0x00], "the marker row moved up by exactly 4 scanlines");
    assert_eq!(px(0, 1), [0, 0, 0, 0]);
    assert_eq!(px(0, H - 1), [0, 0, 0, 0], "the vacated rows are cleared, not left stale");
}

/// The same primitives in shadow mode (`Framebuffer::attach_shadow`,
/// `docs/fb/wc-shadow-plan.md` phase 1): they draw into a RAM shadow and
/// VRAM only ever receives `flush`'s copy of the dirty rectangle. Two RAM
/// buffers stand in for shadow and VRAM, the "VRAM" pre-filled with `0xAA`
/// and with `stride > width`, as in the direct-mode test above.
///
/// What it pins down:
/// 1. inside a batch, VRAM does not change at all;
/// 2. after the outermost `end_batch`, VRAM's visible area equals the
///    shadow's (a nested `end_batch` does not flush early);
/// 3. `stride` padding in VRAM is never written, not even by a scroll;
/// 4. outside a batch, every primitive leaves VRAM up to date by itself —
///    the property that lets callers which never heard of the shadow
///    (`panic.rs`, `draw_boot_screen`, `FBIO_BLIT`, the cursor ISR) work
///    unchanged;
/// 5. a flush copies only the dirty rectangle, not the whole screen.
#[test_case]
fn framebuffer_shadow_mode_flushes_exactly_what_changed() {
    use alloc::boxed::Box;
    use alloc::vec;
    use crate::framebuffer::{Color, Framebuffer};

    const W: usize = 32;
    const H: usize = 24;
    const STRIDE: usize = 40; // deliberately > W
    const BPP: usize = 4;
    const LEN: usize = H * STRIDE * BPP;

    let vram: &'static mut [u8] = Box::leak(vec![0xAAu8; LEN].into_boxed_slice());
    let shadow: &'static mut [u8] = Box::leak(vec![0u8; LEN].into_boxed_slice());
    let vram_base = vram.as_ptr() as usize;
    let shadow_base = shadow.as_ptr() as usize;
    let mut fb = Framebuffer::new(vram, W, H, STRIDE, BPP);

    // SAFETY (both closures): the buffers are leaked, alive for the rest of
    // the boot, and every offset used below is inside `LEN`.
    let vpx = |x: usize, y: usize| -> [u8; 4] {
        let p = (vram_base + (y * STRIDE + x) * BPP) as *const u8;
        unsafe { [*p, *p.add(1), *p.add(2), *p.add(3)] }
    };
    let spx = |x: usize, y: usize| -> [u8; 4] {
        let p = (shadow_base + (y * STRIDE + x) * BPP) as *const u8;
        unsafe { [*p, *p.add(1), *p.add(2), *p.add(3)] }
    };
    let vram_bytes = || -> alloc::vec::Vec<u8> {
        unsafe { core::slice::from_raw_parts(vram_base as *const u8, LEN) }.to_vec()
    };
    let assert_in_sync = |what: &str| {
        for y in 0..H {
            for x in 0..W {
                assert_eq!(vpx(x, y), spx(x, y), "{what}: VRAM differs from shadow at ({x},{y})");
            }
            for x in W..STRIDE {
                assert_eq!(vpx(x, y), [0xAA; 4], "{what}: stride padding at ({x},{y}) was written");
            }
        }
    };

    // Attaching clears the visible VRAM to match the all-zero shadow, and
    // nothing else.
    assert!(fb.attach_shadow(shadow));
    assert!(fb.has_shadow());
    assert_eq!(vpx(0, 0), [0, 0, 0, 0], "attach flushes the black shadow to VRAM");
    assert_in_sync("after attach_shadow");

    // ── (4) Outside a batch: every primitive flushes itself ──────────
    let c = Color::rgb(0x11, 0x22, 0x33);
    let fg = Color::rgb(0xFF, 0xFF, 0xFF);
    let bg = Color::rgb(0x01, 0x02, 0x03);
    fb.fill_rect(2, 3, 5, 4, c);
    assert_eq!(vpx(2, 3), [0x33, 0x22, 0x11, 0x00]);
    assert_in_sync("unbatched fill_rect");
    fb.draw_char(8, 8, b'A', fg, bg, 1);
    assert_in_sync("unbatched draw_char (fast path)");
    fb.draw_char(W - 3, 0, b'B', fg, bg, 1);
    assert_in_sync("unbatched draw_char (clipped path)");
    fb.xor_rect(8, 8, 8, 9);
    assert_in_sync("unbatched xor_rect");
    fb.blit_scaled(&[0x00_44_55_66; 4 * 3], 4, 3);
    assert_in_sync("unbatched blit_scaled");
    fb.fill_rect(0, 4, W, 1, c);
    fb.scroll_up(4);
    assert_eq!(vpx(0, 0), [0x33, 0x22, 0x11, 0x00], "the scroll reached VRAM");
    assert_in_sync("unbatched scroll_up");

    // ── (5) A flush copies the dirty rectangle, not the screen ───────
    // Plant a sentinel straight into VRAM, away from the next primitive.
    // A whole-screen flush would overwrite it with the shadow's pixel.
    let sentinel = (vram_base + (20 * STRIDE + 30) * BPP) as *mut u8;
    unsafe { *sentinel = 0x5A };
    fb.fill_rect(0, 0, 2, 2, c);
    assert_eq!(vpx(30, 20)[0], 0x5A, "a 2x2 fill flushed pixels outside its rectangle");
    unsafe { *sentinel = spx(30, 20)[0] }; // put it back

    // ── (1)/(2) Inside a batch nothing reaches VRAM until the end ────
    let before = vram_bytes();
    fb.begin_batch();
    fb.fill_rect(0, 0, W, H, Color::rgb(0, 0, 0));
    fb.begin_batch(); // nested, as kernel_write_bytes → render_bytes does
    fb.draw_char(0, 0, b'X', fg, bg, 1);
    fb.xor_rect(0, 0, 8, 9);
    fb.fill_rect(0, 12, W, 1, c);
    fb.scroll_up(4);
    fb.end_batch(); // inner: must not flush
    assert!(vram_bytes() == before, "VRAM changed inside a batch");
    fb.fill_rect(5, 5, 3, 3, c);
    assert!(vram_bytes() == before, "VRAM changed inside a batch");
    fb.end_batch(); // outermost: flushes
    assert_eq!(vpx(0, 8), [0x33, 0x22, 0x11, 0x00], "the batched scroll reached VRAM");
    assert_in_sync("after the outermost end_batch");

    // A second attach is refused: the shadow is set once, for good.
    let other: &'static mut [u8] = Box::leak(vec![0u8; LEN].into_boxed_slice());
    assert!(!fb.attach_shadow(other), "attach_shadow must not replace an attached shadow");
}

/// Phase 2 of `docs/fb/wc-shadow-plan.md`: `program_pat` (run by
/// `boot_for_tests`, as by the real boot) made PAT entry 1 WC, left the
/// other seven entries exactly as they were, and the live `IA32_PAT` —
/// read here, not from the recorded status — says so. QEMU's reset PAT
/// has no WC entry and no mapping uses index 1, so anything other than
/// `Programmed` means the scan or the write misbehaved.
#[test_case]
fn pat_entry_1_is_wc_and_nothing_else_moved() {
    use crate::memory::memtype::{pat_program_status, PatProgram};
    use hal::memtype::{pat_entry, MemType, PAT_WC_INDEX};

    let (before, after) = match pat_program_status() {
        Some(PatProgram::Programmed { before, after }) => (before, after),
        Some(other) => panic!("program_pat did not program: {}", other),
        None => panic!("program_pat never ran in boot_for_tests"),
    };
    // SAFETY: IA32_PAT is architectural; reading it has no side effects.
    let live = unsafe { x86_64::registers::model_specific::Msr::new(0x277).read() };
    assert_eq!(live, after, "live IA32_PAT differs from what program_pat reported");
    assert_eq!(pat_entry(live, PAT_WC_INDEX), Some(MemType::Wc));
    for i in (0..8u8).filter(|&i| i != PAT_WC_INDEX) {
        assert_eq!(pat_entry(live, i), pat_entry(before, i), "PAT entry {} changed", i);
    }
    assert!(!hal::memtype::pat_has_wc(before), "QEMU's reset PAT should have no WC entry");

    // A second call reports what is there instead of rewriting it.
    match crate::memory::memtype::program_pat() {
        PatProgram::AlreadyWc { pat } => assert_eq!(pat, live),
        other => panic!("second program_pat: {}", other),
    }
}

/// Phase 3 of `docs/fb/wc-shadow-plan.md`: `set_pat_index_range`, the
/// page-table half of mapping the framebuffer WC, against real page
/// tables. (The test boot has no framebuffer console, so the aperture
/// itself is not retyped here; the real boot logs `framebuffer: ...` and
/// `/proc/fbinfo` shows `fb_wc:`.)
///
/// (1) a 4 KiB mapping ends up selecting PAT index 1, which is WC, keeps
/// its frame, and still reads back what is written through it; (2) a
/// page inside one of the physical window's large leaves is refused,
/// because retyping the leaf would retype its neighbours too; (3) a range
/// whose second page is unmapped is refused *before* the first page is
/// touched — all or nothing.
#[test_case]
fn set_pat_index_range_retypes_4k_leaves_and_refuses_the_rest() {
    use crate::memory::memtype::{leaf_for, set_pat_index_range, RetypeError};
    use hal::memtype::{pat_entry, pat_index, MemType, PAT_WC_INDEX};
    use x86_64::VirtAddr;

    let index_of = |v: u64| {
        let l = leaf_for(VirtAddr::new(v)).expect("mapped");
        let (p, c, w) = l.cache_bits();
        (pat_index(p, c, w), l.phys)
    };

    // A fresh frame nobody else uses, mapped once more through
    // `mmio::map` (4 KiB, PWT|PCD: index 3).
    let frame = unsafe { crate::allocator::phys_alloc(12) }.expect("a frame");
    let virt = unsafe { crate::memory::mmio::map(frame, 4096) }.expect("mmio mapping").as_u64();
    let (before, phys) = index_of(virt);
    assert_eq!(before, 3, "mmio::map is PWT|PCD");
    assert_eq!(phys, frame.as_u64());

    // (3) first, while the page is still index 3: page 2 is unmapped.
    match set_pat_index_range(virt, 2 * 4096, PAT_WC_INDEX) {
        Err(RetypeError::NotMapped { virt: v }) => assert_eq!(v, virt + 4096),
        Err(e) => panic!("expected NotMapped, got: {}", e),
        Ok(_) => panic!("a range with an unmapped page was accepted"),
    }
    assert_eq!(index_of(virt).0, 3, "a refused range changed its first page");

    // (1)
    let r = set_pat_index_range(virt, 4096, PAT_WC_INDEX).expect("retype a 4K leaf");
    assert_eq!((r.pages_4k, r.pages_large, r.old_index), (1, 0, 3));
    let (after, phys_after) = index_of(virt);
    assert_eq!(after, PAT_WC_INDEX);
    assert_eq!(phys_after, frame.as_u64(), "retyping moved the frame");
    // SAFETY: IA32_PAT is architectural; reading it has no side effects.
    let pat = unsafe { x86_64::registers::model_specific::Msr::new(0x277).read() };
    assert_eq!(pat_entry(pat, after), Some(MemType::Wc));
    let p = virt as *mut u64;
    unsafe {
        core::ptr::write_volatile(p, 0x5743_5f4f_4b21_0001);
        core::arch::asm!("sfence", options(nostack, preserves_flags));
        assert_eq!(core::ptr::read_volatile(p), 0x5743_5f4f_4b21_0001);
    }

    // (2) the physical window maps with large pages.
    let window = crate::memory::physical_memory_offset().as_u64() + frame.as_u64();
    let leaf = leaf_for(VirtAddr::new(window)).expect("physical window maps RAM");
    assert_ne!(leaf.page_size, 0x1000, "expected the physical window to use large pages");
    let entry_before = leaf.entry;
    match set_pat_index_range(window, 4096, PAT_WC_INDEX) {
        Err(RetypeError::LargeLeafOutside { .. }) => {}
        Err(e) => panic!("expected LargeLeafOutside, got: {}", e),
        Ok(_) => panic!("retyped a large leaf for a 4K range"),
    }
    assert_eq!(leaf_for(VirtAddr::new(window)).unwrap().entry, entry_before);
}
