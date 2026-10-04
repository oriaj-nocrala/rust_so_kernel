# VFS and Filesystems

Code: `kernel/src/fs/`, crates `vfs/` and `ext2/` (host tests: `cd vfs && cargo test`, `cd ext2 && cargo test` — ext2 has a known temp-file flake, `docs/fs/ext2-test-flake.md`).

## Mounts

| Path | FS | Notes |
|------|----|-------|
| `/` | initramfs | The embedded programs in `/bin`, plus `/etc` (`ETC_FILES`: `localtime` (UTC TZif), `passwd`, `group`, `hosts`, `services`, plus a `resolv.conf` rendered from the DHCP lease on every open). mlibc's `localtime()` **panics** without `/etc/localtime` |
| `/dev` | devfs | Flat, except the hardcoded `/dev/input/` and `/dev/pts/` |
| `/tmp` | ramfs (`vfs::ramfs::RamFs`) | Writable. The only FS with symlink creation *and* socket nodes. `busybox --install -s /tmp/bin` puts the applet symlinks here at boot |
| `/mnt` | ext2 | From the USB stick (read-write, `sync(2)` flushes the stick's cache), else ATA `disk.img` (read-write). Best effort: may be absent |
| `/proc` | procfs | Synthetic, regenerated on every open |

- `ls /` lists the other mounts via `fs::vfs::direct_children`; the mount table redirects traversal into them.
- **procfs contents**: `meminfo`, `stat`, `uptime`, `loadavg`, `cpuinfo`, `<pid>/{stat,statm,cmdline,exe,maps}` (Linux formats, backing `ps`/`top`; `maps` is one line per VMA, `start-end perms 00000000 00:00 0 [stack]`: no file mappings, and `x` shows because anonymous memory is mapped without NX), `self`, `dmesg` (klog), `kdebug`, `fbinfo`, `pci`, `sensors`, `acpi`. Pid listing: `scheduler::all_pids()`, which takes `SCHEDULER` itself, so never call it while holding that lock.

## VFS (`vfs` crate, adapter `kernel/src/fs/vfs.rs`)

- Traits `Inode`/`Filesystem`/`FileHandle`, `MountTable`, `normalize_path`.
- `resolve()` follows symlinks at every component (open/stat). `resolve_no_follow()` leaves the last one (lstat/readlink). Both stop after 8 hops with `ELOOP`.
- Mutations (`create`/`mkdir`/`symlink`/`mksocket`/…) default to `EROFS`; only ramfs and ext2 implement them. `Inode::link_child` (hard links) defaults to `EPERM`; `MountTable::link` checks the mount (`EXDEV`) and that the source is not a directory.
- Open ext2 handles keep their own copy of the inode; the two paths that change a link count (`link_child`, `unlink`) update every live copy (`OPEN_RAWS`), because a handle would otherwise report the old count to `fstat` and write it back on its next write.
- vfs locks call a relax hook (`vfs::lock::set_relax_hook`) so a spinning CPU still answers TLB shootdowns. `vfs::clock::set_clock` gives ramfs wall time.
- **Permissions**: there is no permission model and no uids. `Stat::regular()` reports 0o444; ramfs's `regular_writable()` reports 0o644; ext2 reports the real `i_mode`. The write bits matter because BusyBox `vi` checks `st_mode` as well as `access(W_OK)`.
- **fd table**: 256 fds per process (`EMFILE` beyond). For processes on the console: fd 0 = `/dev/console` (reads come from the keyboard ring), fds 1/2 = `/dev/fb`. Everything written to `/dev/fb` is also mirrored to serial (`[fb] ` prefix) and to klog.

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
- **Block cache** (`hal::blockcache::CachedDevice`, installed by `Ext2Core::mount`): write-through, 4 KiB chunks, CLOCK eviction, up to 64 KiB of read-ahead. The ceiling is an eighth of RAM between 32 and 512 MiB (`hal::blockcache::default_cache_chunks`, chosen in `fs/ext2.rs`; storage is allocated only as it fills, and never given back), and `ext2cache=<MiB>` in `kernel.conf` replaces it (`CachedDevice::set_max_chunks`; shrinking drops the surplus at once). A fixed 32 MiB did not hold two 16 MB Vulkan programs. `/proc/kdebug`: `ext2_cache:` (`held_mib`/`max_mib` too). **Nothing may write a mounted partition except through `Ext2Core::device`**, or the cache goes stale.
  - **Hazard, not fixed:** `State` is a plain `spin::Mutex` held across the device read (`fill`), and `sys_read` reaches it with IF=0 (fd-table lock). A waiter there never answers a TLB shootdown, and with **one CPU** (the `qemu-debug.sh` default) a holder preempted in the read is never run again: a hang within seconds of the first concurrent cold `exec` (gdb: the CPU spinning in `CachedDevice::read_sectors`, IF=0). The USB path (the Ryzen) already runs its transfers with IF=0 and answers shootdowns, so there it is brief. A fix was tried and not kept: a spin hook that runs `tlb::service_pending` plus interrupts off while held (`hal::blockcache::set_irq_hooks`, with `service_pending` in the ATA wait loops) turned the 6-of-6 panics of `scripts/tlb-stress.sh` into 4-of-6 and left no hang, but did not make the natural load (`gui_comp_test`) better and slowed it; the proper fix is not to hold the lock across the device read (an in-flight marker per chunk and a write generation).

## ext2 (`ext2` crate + adapter `kernel/src/fs/ext2.rs`)

- `ext2::Ext2Core` does every on-disk detail: layout, allocation, direct through triple-indirect blocks, directories, fast (<60 B) and slow symlinks, repair passes. It speaks inode numbers and `Ext2Error`, never VFS types.
- The adapter provides the VFS impls, `Ext2Error → Errno`, the `EXT2` global plus `EXT2_LOCK`, and wall-clock time.
- **`EXT2_LOCK` serializes every mutation.** Read paths (`lookup`/`readdir`) don't take it, because mutations call them while holding it and the lock isn't reentrant.
- **Read-only switch**: `READ_ONLY` / `write_lock()` turn every mutation and write-open into `EROFS`, but nothing sets it today (the USB mount used to).
- `rmdir` ends the removed directory's inode with `links_count = 0`, like `unlink`; with 2 left, e2fsck reports a phantom directory (`hw_tests` checks it).
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
