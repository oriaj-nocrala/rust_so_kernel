# Handoff: Files after v1

Status (2026-10-08): Files v1 works on the Ryzen (tested by hand: navigation, text previews, the wheel). Steps 1 (QEMU and the Ryzen #256),
4, 6 and `umask` below are done; not yet on the Ryzen: running programs, PNG previews, imgview, `ui-demo`. Code and state:
`docs/reference/graphics.md` "Files", plan [`files-plan.md`](files-plan.md), the stages that built it
[`../ux/handoff-capabilities-to-files.md`](../ux/handoff-capabilities-to-files.md). Principles: [`../ux/principles.md`](../ux/principles.md)
(P1.1 say why, P2.3 nothing moves under the cursor, P5 one mechanism, P6 capabilities).

## What the user saw on the Ryzen

- **Slow**: opening a folder with many files; navigating, now and then.
- Wanted: **run a program with a double click**, as on Windows (done: step 4).
- A stale `vk_comp` on the stick refused Files (`no such request`). `cargo build` now warns about it (step 6).

## Measured (step 1, QEMU with KVM, 4 CPUs, IDE disk)

`files --bench DIR...` drives itself (open, wait for the preview, Down ×50, PageDown ×10) and prints one summary; the job is
`scripts/metal-jobs/files-bench.sh` (`/mnt/bin`, 2000 empty files in `/mnt/many` and `/tmp/many`, `/mnt/bin` again).

| folder | read_dir | first frame | per key, median / p95 |
|---|---|---|---|
| `/mnt/bin` (97) | 0.6 ms | 4-5 ms | 1.5 / 2.9 ms |
| `/mnt/many` (2000, ext2) | 6.3 ms | 11 ms (was 72) | 1.6 / 7.5 ms (was 62) |
| `/tmp/many` (2000, tmpfs) | 3.5 ms | 12 ms | 1.8 / 11 ms |

- The cause in a big folder was ext2's `lookup`: it built every entry's `String` to find one name, ~3.5 ms per `stat`, ~17 stats
  per page. Fixed with `Ext2Core::find_dir_entry` (`docs/reference/filesystems.md`). The UI itself (render + paint) is a few ms.
- A preview costs ~600 ms the first time (reading `cap-exec` + `files-preview` from disk) and ~18 ms after (block cache; ~10 ms
  of it is `spawn`, which waits for the exec).
- **Ryzen #256** (under `vk_comp`, stick over USB-MSC), median per key / first frame: `/mnt/bin` 1.4-4.0 / 8-11 ms,
  `/mnt/many` 7.1 / 8.3 ms, `/tmp/many` 11.1 / 12.5 ms (render ≤ 1 ms of it). The p95 is ~30 ms in every folder, tmpfs included:
  two vblanks of the compositor's pacing, not Files' work (the frame waits for its buffer back). A cold preview is 65 ms over USB
  (cap-exec + files-preview read once), warm ~17-24 ms (~10 of it `spawn`). Making 2000 files on the stick took 75 s.

## Work left, in order

1. **Per-frame p95 of ~30 ms under `vk_comp`** (above): a frame that waits two vblanks for its buffer. Belongs to the
   compositor's pacing (`docs/gpu/g5-layer4-handoff.md`, memory "Compositor CPU window cost"), not to Files.
2. **Page cache for files that do not change** (also the linking policy's prerequisite, memory note "Linking policy"): pages of a
   file read once stay in memory, shared by every reader; `exec` and `mmap` of the same file map the same frames (read-only, COW
   on a private write). A preview followed by opening the file reads the file once; two processes running the same binary share
   its text. Design it in `mm`/`vfs` (host-tested), invalidate on write/truncate (ext2 is RW), bound it by the buddy allocator's
   free memory. Start with a design doc (`docs/memory/` or `docs/fs/`). For Files alone it saves ~45 ms on the Ryzen, once per
   binary (600 ms in QEMU's IDE): worth it for the linking policy and shared text more than for previews.
3. **Keep the provider alive** only if a ~17 ms warm preview ever matters (it does not today, the selection settles 120 ms first): one `files-preview` per Files
   window, fed a new file fd + memfd per request over a socketpair (`SCM_RIGHTS`; it keeps capability mode). Weigh it against
   P6.4: a long-lived decoder that saw a hostile file stays compromised for the next files; respawn per folder at least.
4. **"Opening = a bigger preview"** (the user's idea): today a preview is a sandboxed decoder's picture, opening runs an app;
   Quick Look (Space) shows the preview over the whole window. Whether a viewer app should take over from the preview without
   reading the file again belongs with step 2 — keep the decoder and the app separate processes (P6.4).
5. Small leftovers: `umask` exists (syscall 95, mlibc sysdep, `umask_test`) but nothing applies it, because `open`/`mkdir` keep no
   creation mode; the protocol has no version negotiation (an old compositor disconnects a client using a newer request); a
   console program is recognised only by not being a launcher/open-with command (`files::program::windowed`); multi-selection, a
   per-folder pinned layout, `filesd` (Files v2).

## Found on the way (fixed)

- `execve` of a `#!` script was not supported and `/bin/sh` did not exist: a script run outside ash failed (exit 126 in `term -e`).
- mlibc's `pipe2` refused every flag; BusyBox `mkdir -p` failed with `EROFS` on `/tmp/` and `/mnt/` (a mount point over the
  read-only root).
- `gui-e2e.sh`: a long `mv` let the click land before the last PS/2 packets (QEMU queues them), on whatever window was there; it
  now moves in steps of 127.

## How to test

`cd files && cargo test`; `scripts/gui-e2e.sh files` (the long one: five Files launches, F7 runs a script through `term -e`,
~10 min under TCG); `ui`, `wm`, `demo` for the libraries; `scripts/run-abi-suite.sh` for kernel changes; the bench job above for
speed (QEMU: write it to `/autorun/job` of a copy of `disk.img`, `metal-run` skill). Deploy: `scripts/deploy-usb-boot.sh` (kernel)
and `scripts/sync-usb-data.sh` (data) — and rebuild the NVK programs when `cargo build` says they are stale.
