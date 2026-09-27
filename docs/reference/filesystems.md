# VFS and Filesystems

Code: `kernel/src/fs/`, crates `vfs/` and `ext2/` (host tests: `cd vfs && cargo test`, `cd ext2 && cargo test` — ext2 has a known temp-file flake, `docs/fs/ext2-test-flake.md`).

## Mounts

| Path | FS | Notes |
|------|----|-------|
| `/` | initramfs | The embedded programs in `/bin`, plus `/etc` (`ETC_FILES`: `localtime` (UTC TZif), `passwd`, `group`). mlibc's `localtime()` **panics** without `/etc/localtime` |
| `/dev` | devfs | Flat, except the hardcoded `/dev/input/` and `/dev/pts/` |
| `/tmp` | ramfs (`vfs::ramfs::RamFs`) | Writable. The only FS with symlink creation *and* socket nodes. `busybox --install -s /tmp/bin` puts the applet symlinks here at boot |
| `/mnt` | ext2 | From the USB stick (read-only), else ATA `disk.img` (read-write). Best effort: may be absent |
| `/proc` | procfs | Synthetic, regenerated on every open |

- `ls /` lists the other mounts via `fs::vfs::direct_children`; the mount table redirects traversal into them.
- **procfs contents**: `meminfo`, `stat`, `uptime`, `loadavg`, `cpuinfo`, `<pid>/{stat,statm,cmdline,exe}` (Linux formats, backing `ps`/`top`), `self`, `dmesg` (klog), `kdebug`, `fbinfo`, `pci`, `sensors`, `acpi`. Pid listing: `scheduler::all_pids()`, which takes `SCHEDULER` itself, so never call it while holding that lock.

## VFS (`vfs` crate, adapter `kernel/src/fs/vfs.rs`)

- Traits `Inode`/`Filesystem`/`FileHandle`, `MountTable`, `normalize_path`.
- `resolve()` follows symlinks at every component (open/stat). `resolve_no_follow()` leaves the last one (lstat/readlink). Both stop after 8 hops with `ELOOP`.
- Mutations (`create`/`mkdir`/`symlink`/`mksocket`/…) default to `EROFS`; only ramfs and ext2 implement them.
- vfs locks call a relax hook (`vfs::lock::set_relax_hook`) so a spinning CPU still answers TLB shootdowns. `vfs::clock::set_clock` gives ramfs wall time.
- **Permissions**: there is no permission model and no uids. `Stat::regular()` reports 0o444; ramfs's `regular_writable()` reports 0o644; ext2 reports the real `i_mode`. The write bits matter because BusyBox `vi` checks `st_mode` as well as `access(W_OK)`.
- **fd table**: 16 fds per process. For processes on the console: fd 0 = `/dev/console` (reads come from the keyboard ring), fds 1/2 = `/dev/fb`. Everything written to `/dev/fb` is also mirrored to serial (`[fb] ` prefix) and to klog.

## Timestamps

- ext2 stamps real times through the adapter (`stamp_new`/`stamp_modified`/`stamp_changed`, using `time::now_unix_secs`).
- A directory's mtime/ctime is written **last** in each operation (`touch_dir`, read fresh), because some operations write a stale copy of the directory's record first.
- ramfs keeps atomic `Times` per node.
- No atime updates on read (noatime).
- Filesystems without times report the boot time (`fill_missing_times`), not 1970.
- `fstat` on an ext2 directory returns the record read at open.
- Test: `fstime_test`.

## Block devices

- Seam: `hal::block::BlockDevice` (512-byte sectors). Implementations:
  - `AtaBlockDevice` (`kernel/src/block/`): ATA. Not itself seamed onto `PortIo`.
  - `UsbBlockDevice` (`kernel/src/block/usb.rs`, USB mass storage).
  - `hal::block::MemDisk`: RAM, for tests.
- `hal::block::Partition` adds an offset and **refuses** any request outside its window (never clamps).
- **Block cache** (`hal::blockcache::CachedDevice`, installed by `Ext2Core::mount`): write-through, 4 KiB chunks, 32 MiB cap, CLOCK eviction, up to 64 KiB of read-ahead. `/proc/kdebug`: `ext2_cache:`. **Nothing may write a mounted partition except through `Ext2Core::device`**, or the cache goes stale.

## ext2 (`ext2` crate + adapter `kernel/src/fs/ext2.rs`)

- `ext2::Ext2Core` does every on-disk detail: layout, allocation, direct through triple-indirect blocks, directories, fast (<60 B) and slow symlinks, repair passes. It speaks inode numbers and `Ext2Error`, never VFS types.
- The adapter provides the VFS impls, `Ext2Error → Errno`, the `EXT2` global plus `EXT2_LOCK`, and wall-clock time.
- **`EXT2_LOCK` serializes every mutation.** Read paths (`lookup`/`readdir`) don't take it, because mutations call them while holding it and the lock isn't reentrant.
- **Read-only mount** (from USB, `init_read_only`): no repair passes, and every mutation / write-open returns `EROFS`.
- Test images: `ext2::testimg::{build_minimal_image, build_image_with_orphans}`, shared with `hw_tests`.

### Crash safety (there is no journal)

- Every mutation writes in the order "allocate and write content, then link". A crash can leak blocks, never leave a dangling reference.
- `unlink`/`rmdir`: write the zeroed inode record (`write_inode`) **before** clearing its bitmap bit. `i_dtime` must be a real epoch time; a small value looks to e2fsck like an orphan-list link.
- **Repair passes at read-write mount**, before `/mnt` is exposed:
  - `reconcile_free_counts` recomputes the superblock/BGD counters from the bitmaps.
  - `reclaim_orphans` frees used-but-unreachable inodes and blocks, like e2fsck passes 1–4. It frees inodes first, then blocks, and zeroes and dtime-stamps each reclaimed record.
- **Ordering invariant in `reclaim_orphans`**: run the reachability walk from root *before* pre-marking the reserved inodes (`1..first_ino`, which includes root). `mark_reachable` treats an already-marked inode as visited, so pre-marking root once made the whole tree look unreachable and freed live data.
  - Also always reserve the superblock block and the backup superblock/BGDT slots in every group.
- Out of scope: an inode whose bitmap bit is already clear but whose record still has content (the `disk.img` "phantom orphan", made by build.rs's `debugfs mkdir`).
- Oracle tests: `ext2::repair` runs `e2fsck -fn` against real `mke2fs`/`debugfs` images.
