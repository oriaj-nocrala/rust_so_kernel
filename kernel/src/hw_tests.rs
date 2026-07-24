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
