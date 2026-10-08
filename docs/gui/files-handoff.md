# Handoff: Files after v1

Status (2026-10-08): Files v1 works on the Ryzen (tested by hand: navigation, text previews, the wheel). Not tried there yet:
PNG previews, opening a PNG with imgview, `ui-demo`, the ABI suite of the capability stages. Code and state: `docs/reference/graphics.md`
"Files", plan [`files-plan.md`](files-plan.md), the stages that built it [`../ux/handoff-capabilities-to-files.md`](../ux/handoff-capabilities-to-files.md).
Principles: [`../ux/principles.md`](../ux/principles.md) (P1.1 say why, P2.3 nothing moves under the cursor, P5 one mechanism, P6 capabilities).

## What the user saw on the Ryzen

- **Slow**: opening a folder with many files; navigating, now and then. Not yet measured: do not assume it is the stick.
- Wanted: **run a program with a double click**, as on Windows.
- A stale `vk_comp` on the stick refused Files (`no such request`: it predated `semantics_node`). `vk_comp` is built outside
  `cargo build` (`probes/nvk/build.py`, then `strip -o disk-image-root/bin/vk_comp ~/src/gpu-ref/nvk-probe/vk-comp`), so nothing
  warned. Fixed by rebuilding; the guard below is still missing.

## Work, in order

1. **Measure before fixing** (`metal-run` skill, `feedback: name the failure before varying load`). Files logs per step with
   `Instant`: `read_dir` + sort (count, ms), the first frame after a folder change (render + paint + `set_semantics`), each
   preview (spawn → exit → decode, ms), per-row `stat` (count per frame). Job: `files` on `/mnt/bin` and a folder of 2000 files
   made with `touch`, arrow down 50 rows, PageDown 10 times; print one summary at the end. Run it in QEMU (KVM) first for a
   baseline, then on the Ryzen. Suspects, cheapest first:
   - Every preview execs `cap-exec` (545 KB) + `files-preview` (574 KB) from the stick: no page cache, so each is read again
     over USB-MSC.
   - `ui::State::render` rebuilds every row's `Row` (strings) and `text` lays out each cell every frame; `Measure::width` calls
     `parley` for right-aligned cells.
   - A folder change re-reads the directory and re-`stat`s visible rows from ext2 on USB (block cache only).
2. **Page cache for files that do not change** (also the linking policy's prerequisite, memory note "Linking policy"): pages of a
   file read once stay in memory, shared by every reader; `exec` and `mmap` of the same file map the same frames (read-only, COW
   on a private write). This is the "COW" the user asked about: a preview followed by opening the file reads the file once;
   two processes running the same binary share its text. Design it in `mm`/`vfs` (host-tested), invalidate on write/truncate
   (ext2 is RW now), bound it by the buddy allocator's free memory. Start with a design doc (`docs/memory/` or `docs/fs/`).
3. **Keep the provider alive** only if step 1 shows spawn cost matters after the page cache: one `files-preview` per Files
   window, fed a new file fd + memfd per request over a socketpair (`SCM_RIGHTS`; it keeps capability mode, so it still opens
   nothing). Kill and respawn on crash/timeout (the current reasons stay). Weigh it against P6.4: a long-lived decoder that saw a
   hostile file stays compromised for the next files; respawn per folder at least.
4. **Run programs with a double click / Enter** (`files::open`): a regular file with an `x` bit whose first bytes are
   `\x7fELF` or `#!` is run like the panel's launcher (no sandbox, `GUI_DISPLAY` inherited). Put the "is it a program" test in
   the `files` library (host tests: ELF, script, non-executable, directory). **Open question for the user:** a console program
   (`hello`, a script) has no window and its output is lost; Windows opens a console. Option: `term -e PROG [ARG]...` (term runs
   the command instead of ash and keeps the window until a key), then Files runs console programs through it. Ask before building.
5. **"Opening = a bigger preview"** (the user's idea): today a preview is a sandboxed decoder's picture, opening runs an app.
   Quick Look (Space) already shows the preview over the whole window. Deciding whether a viewer app should take over from the
   preview without reading the file again belongs with step 2 (shared pages make the second read free) — keep the decoder and
   the app separate processes (P6.4).
6. **Guard the out-of-tree binaries**: `cargo build` warns when `disk-image-root/bin/vk_comp` (and the other NVK programs) is
   older than `gui/src`, `vk-comp/src` or `nvgpu/uapi/nvgpu.h` (root `build.rs`, a `cargo:warning`), so a stale compositor never
   reaches the stick unnoticed. Mention it in the `gpu-g5` skill.
7. Small leftovers: `umask` (no syscall 95, no mlibc sysdep: BusyBox `mkdir -p` prints an mlibc `__ensure`), the protocol has no
   version negotiation (an old compositor disconnects a client using a newer request; a `get_version`-style handshake or optional
   requests would let clients degrade), multi-selection, a per-folder pinned layout, `filesd` (Files v2).

## How to test (unchanged)

`cd files && cargo test`; `scripts/gui-e2e.sh files` (the long one: four Files launches, ~10 min under TCG); `ui`, `wm`, `demo`
for the libraries; `scripts/run-abi-suite.sh` for kernel changes. Deploy: `scripts/deploy-usb-boot.sh` (kernel) and
`scripts/sync-usb-data.sh` (data) — plus `vk_comp` restaged by hand when `gui` changed.
