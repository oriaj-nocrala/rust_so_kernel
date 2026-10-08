# Handoff: from capabilities to the file manager (v1)

Status: **stages 1-4 done** (openat2, NX, rights on fds, capability mode; 2026-10-08); next: stage 5 (`cap-exec`; exec is denied in capability mode, so it needs an "enter at exec" step or `fexecve`). Written 2026-10-08. For a fresh agent session: everything needed is
here or linked. Work stage by stage; each stage ends in a commit and leaves everything that
worked still working. Estimates are guesses in sessions (one long working session each).

Goal: the cornerstone (kernel capabilities, steps 1-5 of
[`../ai/capabilities-plan.md`](../ai/capabilities-plan.md)), then the first app built on the
principles: Files v1 ([`../gui/files-plan.md`](../gui/files-plan.md)) with sandboxed preview
providers, on a shared widget crate that emits the semantic tree.

## Read first

- `CLAUDE.md` (invariants: lock order, fd-table lock before `SCHEDULER`, IF rules, TLB rules).
- [`principles.md`](principles.md): P5 (one mechanism; look for the existing one first), P6
  (capabilities), P7 (semantic tree), P1.1 (errors with causes).
- [`../ai/capabilities-plan.md`](../ai/capabilities-plan.md) (design sketch: Capsicum semantics).
- [`../gui/files-plan.md`](../gui/files-plan.md) and [`backbones.md`](backbones.md) B3, B5.
- Skills: `linux-abi` (raw C test + sabotage recipe), `kernel-testing` (which suite covers
  which crate; prove tests by sabotage), `userspace-programs` (getting programs onto
  `disk.img`), `qemu-debug` (headless runs, `gui-e2e.sh`).

## Rules of the road

- **Prove every test by sabotage**: break the check, see the test fail, restore.
- **Fail fast**: `scripts/run-abi-suite.sh` (one boot, all raw C tests); never wait for a full
  QEMU timeout. Run it at the start (baseline) and at the end of every kernel stage.
- **Host tests for crate logic** (`cd vfs && cargo test`, `cd gui && cargo test`); a root
  `cargo build` proves nothing for those crates.
- **Docs in the same change**: `docs/reference/syscalls.md` for every syscall touched,
  `docs/reference/graphics.md` for GUI pieces, and the plan docs' status lines.
- **Known traps** (from memory of earlier sessions): if `disk.img` is full, `debugfs` copies
  truncated binaries and they crash at start (`dumpe2fs -h disk.img | grep Free`); after a
  C-side build failure `run-abi-suite.sh` may run the old binary; an ABI-suite stall after a
  `fork` is an open bug (the kernel prints `STRANDED READY` if it recurs: report it with the
  log, don't work around it).
- Commit at the end of each stage (message ends with the repo's co-author line); no push.

## Stage 0. Baseline (0.25)

`cargo build`; `scripts/run-abi-suite.sh` green; `cd vfs && cargo test`;
`cd gui && cargo test`; `scripts/gui-e2e.sh` passes. Write down anything already failing
before touching code.

## Stage 1. `openat2` + `RESOLVE_BENEATH` (1-1.5)

Linux-numbered (437), useful alone, the core of capability mode.

- `struct open_how { u64 flags; u64 mode; u64 resolve; }` with a `size` argument (`E2BIG` for a
  larger struct with nonzero tail, `EINVAL` for unknown `resolve` bits or a too-small size).
- Support `RESOLVE_BENEATH` (no `..` above the dirfd, no absolute paths, no absolute symlink
  targets, no symlink that escapes) and `RESOLVE_NO_SYMLINKS`. Others → `EINVAL` for now; list
  them in `syscalls.md`.
- **Where:** today a `dirfd` resolves through the *path string* the fd was opened by
  (`FileDescriptorTable::path`, `kernel/src/process/file.rs`; `resolve_path` in
  `kernel/src/process/syscall/mod.rs`; `sys_openat` in `syscall/fs.rs`). A string-prefix check
  is wrong (symlinks, `..` after a symlink). Do the check in the walk: `vfs/src/mount.rs`
  `resolve_inner` gains a "beneath this directory" bound that every `..` and every symlink
  target is checked against. That logic is host-testable: test it in `vfs` first.
- **Tests:** vfs host tests (`..` at the root of the bound, `a/../..`, symlink to `/etc`,
  relative symlink climbing out, symlink inside staying inside, mount point under the bound);
  a raw C test `openat2_test` (`linux-abi` recipe) for the syscall surface; sabotage each.

## Stage 2. NX for user pages + `PROT_EXEC` (1)

Also a prerequisite of dynamic linking (`docs/userland/roadmap.md` step 2), so W^X holds
from the first dynamically linked binary.

- Facts: `EFER.NXE` is already on (bootloader on the BSP, AP trampoline sets it,
  `kernel/src/cpu/init.rs` verifies it); kernel PTEs use bit 63. User mappings just never set it:
  `kernel/src/memory/elf_loader.rs` (~line 513, comment says NX is "not confirmed": stale),
  anonymous/memfd `mmap`, `mprotect` (`PROT_EXEC` ignored, `docs/reference/syscalls.md`),
  the user stack. The comment at `kernel/src/memory/page_table_manager.rs:21` is stale too.
- Set `NO_EXECUTE` on segments without `PF_X`, on mappings without `PROT_EXEC`, on stacks;
  honour `PROT_EXEC` in `mmap`/`mprotect`.
- **Must stay executable:** the signal trampoline page (`memory::signal_trampoline`,
  `TRAMPOLINE_VA`); check `vsyscall`/vDSO-like pages if any; huge-page paths; COW copies
  keep the bit; fork copies keep it.
- **Gate:** the ABI suite, BusyBox, DOOM/Quake, `term`, the compositor and (if available on the
  host) a std/tokio probe (`scripts/run-std-probe.sh`) still run; a new C test jumps into a
  non-`PROT_EXEC` page and gets `SIGSEGV`, then `mprotect(PROT_EXEC)` makes it run; sabotage.
- **Say why (P1.1):** the page fault's error code has the instruction-fetch bit; a process killed
  for executing non-executable memory is announced as such ("executed non-executable memory at
  0x..."), not as a plain segmentation fault, in the kill notice (`kalert!`) and the log.

## Stage 3. Rights on fds + `cap_rights_limit` (1.5)

- A rights mask per fd-table entry (read, write, seek, mmap, ioctl, fstat, lookup-under,
  create-under, unlink-under, accept, connect, ...; start with what the syscalls below need,
  Capsicum's names). `cap_rights_limit` only narrows; `dup`, `fork`, `exec`, `SCM_RIGHTS`
  (`usock`) keep the mask.
- **Numbers:** Linux has no Capsicum calls. Pick a documented constanos range (or `prctl`
  options) and record it in `syscalls.md` with the reason.
- **Where:** fd table in `kernel/src/process/file.rs`; checks in the syscall layer before
  `FileHandle::read/write/...`. Mind CLAUDE.md: the fd-table lock comes before `SCHEDULER`; use
  `syscall::with_files`/`with_fd_table`. Put the pure "may this op run with these rights" table
  in a host-testable place (`vfs` or a small new crate per the CLAUDE.md crate pattern).
- **Tests:** host tests for the table; a C test per narrowed right (a read-only fd refuses
  `write`, a no-lookup dirfd refuses `openat`, a mask survives `dup`/`fork`/`SCM_RIGHTS`);
  sabotage.

## Stage 4. Capability mode: `cap_enter` + `ECAPMODE` (1.5-2)

- A per-process flag, inherited by `fork`/`clone`/`exec`, never cleared.
- **First write the audit**: every syscall in the table (`kernel/src/process/syscall/mod.rs`)
  classified allowed / denied / fd-relative-only, as a table in `syscalls.md`. In cap mode:
  `open`/`openat` with `AT_FDCWD` or an absolute path → `ECAPMODE`; `openat` relative to a held
  dirfd behaves as `RESOLVE_BENEATH` (stage 1); path-based `bind`/`connect`, `kill` of a pid
  (use `pidfd`), `chdir`, mount, `/dev` opens → denied.
- Decide and document `/proc/self` (deny by default; the open question in the capabilities plan).
- **Test:** one C program enters cap mode and tries each escape (`/etc/...`, `/dev/...`, `..`
  from its dirfd, `kill` of another pid, connecting to a socket path); every attempt fails with
  `ECAPMODE`/`ENOTCAPABLE`; sabotage each check.
- **Say why (P1.1):** Capsicum's errors are opaque to the program. Record every denial (syscall,
  path or target, the capability or right that was missing) in a per-process ring readable from
  **outside** the process (`/proc/<pid>/...` or the kernel log), because foreign programs will
  only print "Not permitted". The C test also checks that each denial left its record.

## Stage 5. `cap-exec`, the launcher (0.5)

- A small static program: `cap-exec [--dir PATH[:rights]]... [--fd N:rights]... -- prog args`.
  Opens what it is told, limits rights, sets cap mode, `exec`s (or an exec flag that enters cap
  mode after load). This is the one way services, preview providers and later generated apps
  are started (P5): don't add another.
- **Test:** `cap-exec --dir /tmp/x -- cat /etc/hostname` fails; reading a file under `/tmp/x`
  through the passed dirfd works.

## Stage 6. A Rust std GUI client + a build path for std programs (1)

- Today clients use `userspace::gfx` (`no_std`, raw syscalls: connect to `$GUI_DISPLAY`, memfd
  pool, `SCM_RIGHTS`, events). The userland roadmap moves programs to Rust std on musl
  (`docs/userland/roadmap.md` step 6). The file manager is written in std from the start.
- The `gui` (wire format, protocol), `text`, `draw`, `img` crates are `no_std + alloc`: usable
  from std as they are (`vk-comp` already uses `gui` from std).
- Write the client side in std, reusing `gui::wire`/`protocol` (no second encoder, P5). fd
  passing: nightly's `unix_socket_ancillary_data` or raw `sendmsg` via `libc`. Place it as its
  own crate/workspace like the others.
- **Build path:** there is no list for std disk programs in `kernel/build.rs` yet (`vk-comp` is
  a special staticlib linked with NVK). Read `scripts/run-std-probe.sh` for how std probes are
  built and copied, then add the smallest general mechanism (a list of std crates built for
  `x86_64-unknown-linux-musl` and copied to `disk-image-root/bin/`), documented in the
  `userspace-programs` skill.
- **Gate:** a std "hello window" opens under `compositor` in QEMU and receives keys and pointer.

## Stage 7. Widget crate with the semantic tree (2)

- Every app draws its own buttons today (`panel.rs` is 809 lines). One crate (e.g. `ui/`,
  `no_std + alloc`, host-tested like `gui`): label, button, single-line text field, list with
  virtualized rows (keyboard, wheel, selection, type-to-find), scroll area, split pane, sidebar.
  Looks come from `gui::theme` (Luna/9x), not new colours.
- **Semantic tree (B5):** every widget yields a node: id (stable across frames), role, name,
  value/state, bounds, actions. Use AccessKit's schema; depend on the `accesskit` crate if it
  builds for this target, else mirror its types (record which).
- **Publishing the tree:** a new `gui` protocol message (client → compositor) carrying the
  tree per surface; the compositor keeps the latest and answers a dump request that
  `gui-e2e.sh` (and later `agentd`) uses. Keep `gui/tests/c_wire.rs` green (C clients ignore it).
- **Tests:** host tests for layout, list virtualization (10 000 rows render only the visible
  ones), keyboard navigation, tree contents and id stability; sabotage.
- Follow-up, not in this plan: move `panel` onto `ui` (P5).

## Stage 8. Files v1 (2-3)

Per [`../gui/files-plan.md`](../gui/files-plan.md) step 1:

- Sidebar (places: `/`, `/mnt`, `/tmp`, `/proc`, `/dev`), path bar, list (name, size, type,
  modified; `std::fs::read_dir` + metadata only for visible rows), inspector pane.
- Keyboard: arrows, Enter (open dir / open with app), Backspace (up), Space (full preview),
  type-to-find. Mouse: click, double-click, wheel (first check that USB/PS2 wheel arrives as
  `EV_REL`/`REL_WHEEL` through the compositor; if not, that is a small stage of its own).
- **Preview providers** (P6.4): a separate program `files-preview`, launched through `cap-exec`
  with only the file's fd (read rights) and an output memfd; it writes ARGB pixels + metadata.
  PNG through `img`, text/code through `text`. A provider that crashes or hangs (timeout)
  leaves the app working and shows why in the inspector (P1.1, P6.5).
- **Preview layout per folder**, never per selection (P2.3).
- **Open with:** reuse the launcher's list `/mnt/etc/gui/apps`; add the smallest mapping from
  file type to app (a text file next to it), not a registry.
- Install it in the launcher list; on `disk.img`.
- **Gate (`gui-e2e.sh` new mode `files`):** the list shows `/mnt/bin` entries; navigation by
  keys; a PNG preview appears in the inspector; the tree dump contains the rows by name; killing
  the provider mid-preview leaves the app responsive with an error message; a provider trying to
  open another path fails (`ECAPMODE`). Then a look on the Ryzen by hand (`metal-run` skill if
  unattended).

## After this plan (not now)

- Files v2: copy/move/delete through a `filesd` daemon, undo, shell equivalents (ideas F6, U7).
- B1 (VFS journal): live refresh, search, directory sizes. IOMMU (only needed for userland
  drivers). `agentd` reading the tree.

## Totals

About 12-15 sessions: capabilities 5.5-7 (stages 1-5), GUI groundwork 3 (stages 6-7), Files v1
2-3 (stage 8), plus baseline and slack.
