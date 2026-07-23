# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build and Run

Requires Rust **nightly** toolchain (set in `rust-toolchain.toml`). Requires `qemu-system-x86_64` installed.

```bash
# Build + launch in QEMU (UEFI, 512 MB RAM, serial to stdout)
cargo run

# Build only (kernel binary + disk image)
cargo build

# Build the kernel crate alone (bare-metal target)
cd kernel && cargo build --target x86_64-unknown-none
```

The top-level `cargo run` builds the kernel ELF, wraps it in a UEFI disk image via the `bootloader` crate, then spawns `qemu-system-x86_64`. Serial output appears in the terminal. The kernel ELF is built by `build.rs` shelling out to a **nested** `cargo build` (not cargo's artifact-dependency/`bindeps` feature) — see `build_kernel()` in the root `build.rs` for why (`bindeps` + `-Z build-std` panics inside cargo itself on every nightly tested).

### Headless interactive debugging (no display/keyboard)

For non-interactive sessions (agents, CI) that need to type into the shell and read output — not just watch `cargo run`'s serial stream — use `scripts/qemu-debug.sh` instead of hand-building a `qemu-system-x86_64` command line or a one-off key-sending script. It wraps the whole flow: headless boot (`-display none`, serial to a log file, monitor over a unix socket, `-d int` exception trace), a `sendkey`-based `send "text"` that maps characters to QEMU keynames (including shift-combos) and paces them so the PS/2 ISR doesn't drop events, plus `screendump`/`log`/`wait-for`. See the script's header comment for the full subcommand list and an example session. Background/monitor-socket gotchas are recorded in the `debugging-technique-qemu-monitor` memory.

```bash
scripts/qemu-debug.sh start                                    # cargo build + launch headless
scripts/qemu-debug.sh wait-for "About to start first process"  # poll serial.log instead of a blind sleep
scripts/qemu-debug.sh send "busybox ash" && scripts/qemu-debug.sh enter
scripts/qemu-debug.sh log 50                                   # tail serial.log
scripts/qemu-debug.sh stop
```

### QEMU integration tests

Real hardware-path behavior (drivers that need actual QEMU devices, not just host-testable
pure logic — see `hal/`'s host tests via `cd hal && cargo test`, 64 tests, <1s, no QEMU) is
asserted by a `#![feature(custom_test_frameworks)]` harness that boots the real kernel in
QEMU and reports PASS/FAIL as a process exit code:

```bash
scripts/run-kernel-tests.sh
```

This builds `kernel`'s test binary (`cargo build --target x86_64-unknown-none --tests`, run
from `kernel/`), boots it headless in QEMU with `-device isa-debug-exit`, and exits 0 (every
`#[test_case]` passed) or nonzero (a test failed, or the kernel hung/crashed before reporting)
— see the guest side (`kernel/src/test_framework.rs`, `kernel/src/hw_tests.rs`,
`kernel/src/init/test_support.rs`) and host side (`qemu-test-runner/`, a standalone crate).

**Plain `cargo test --target x86_64-unknown-none` does not work here** — verified, not
assumed: it builds the `kernel` bin target twice in one invocation (once normally, once under
`--cfg test`), and with this crate's `-Z build-std`, that produces two independently-built
`core` crates that collide (`error[E0152]: duplicate lang item in crate 'core': 'sized'`) the
moment a shared dependency needs both. `cargo build --tests` doesn't hit this, so
`scripts/run-kernel-tests.sh` drives that instead of `cargo test` itself — see that script's
header comment and `kernel/.cargo/config.toml`'s `[target.x86_64-unknown-none] runner`
comment for the full diagnosis. `[acpi] SELFTEST` (`kernel/src/acpi.rs`, the boot-time ACPI
self-check against known QEMU i440fx values) is the first real test case
(`kernel/src/hw_tests.rs::acpi_selftest_passes`). See `docs/drivers/architecture.md`'s
Testing section and `docs/drivers/roadmap.md`'s Phase 2 for more.

## Two-Crate Workspace

| Crate | Path | Purpose |
|-------|------|---------|
| `so2` | `/` (host) | Build script + QEMU launcher |
| `kernel` | `kernel/` | Bare-metal kernel (`#![no_std]`, `x86_64-unknown-none`) |

The host crate's `build.rs` creates a UEFI boot image; `src/main.rs` only launches QEMU with the image paths injected by the build script.

Kernel crate config in `kernel/.cargo/config.toml` enables `-Z build-std` to rebuild `core`/`alloc`/`compiler_builtins` for the bare-metal target.

## Boot Sequence (`kernel/src/init/mod.rs`)

`kernel_main` → `init::boot`:
1. `devices::init_idt()` — load IDT (exceptions, PIC IRQs); syscalls go through the `syscall` instruction (MSR LSTAR, wired later in `process::tss::init()`), not an IDT gate
2. Framebuffer setup (inline, requires `&'static mut` lifetime from BootInfo)
3. `memory::init_core()` — store physical memory offset, seed Buddy allocator
4. `memory::test_allocators()` — smoke test slab + Vec + String
5. `devices::draw_boot_screen()`
6. `devices::init_hardware_interrupts()` — init PIC + PIT (preemptive timer)
6b. `mouse::init()` — best-effort PS/2 auxiliary device enable (IRQ12); bounded polls, never hangs boot on hardware with no PS/2 mouse
6c. `ac97::init()` — best-effort PCI AC97 audio codec enable; bounded polls, never hangs boot on hardware/QEMU configs with no AC97 device
7. REPL initial prompt
8. `process::tss::init()` — TSS + GDT (needed for ring-3 → ring-0 stack switch)
9. `processes::init_all()` — create idle, user, and shell processes
10. `process::start_first_process()` — enable interrupts, jump to first trapframe

## Memory Subsystem (`kernel/src/memory/`, `kernel/src/allocator/`)

**Physical allocator:** Buddy allocator (`allocator/buddy_allocator.rs`), orders 12–28 (4 KiB–256 MiB). Single global `BUDDY: Mutex<BuddyAllocator>` is the **sole** owner of physical frames after boot. Uses a compile-time O(1) bitmap (covers 0–512 MiB) for fast free-block lookup.

**Heap allocator:** Slab allocator (`allocator/slab.rs`) backed by Buddy. Registered as the global `#[global_allocator]`, enabling `alloc` (Vec, Box, String, etc.) throughout the kernel.

**Page tables:** `OwnedPageTable` (`memory/page_table_manager.rs`) wraps `x86_64::OffsetPageTable`. Kernel address space uses `from_current()` (captures CR3); new user spaces use `new_user()` which clones kernel mappings into a fresh PML4.

**Address space:** `AddressSpace` (`memory/address_space.rs`) bundles an `OwnedPageTable` + `VmaList`. Each `Process` owns one.

**VMAs** (`memory/vma.rs`): Up to 16 VMAs per process. Two kinds: `Code` (pre-loaded, not demand-paged) and `Anonymous` (zero-filled on demand — stack, heap).

**Demand paging** (`memory/demand_paging.rs`): Page fault handler (in `init/devices.rs`) reads CR2, finds the faulting VMA, calls `map_demand_page` to allocate a physical frame from Buddy, zero it, and map it. Kernel-mode faults panic; user-mode faults outside any VMA kill the process.

**ELF loader** (`memory/elf_loader.rs`): Parses ELF64 PT_LOAD segments, maps them into a fresh `AddressSpace`, zeros BSS, and registers demand-paged stack. Static executables only (no dynamic linker). `build_initial_stack` writes a real, dynamically-sized SysV ABI initial stack frame (argc/argv/envp/auxv) onto the pre-mapped top stack page — sized from whatever `sys_exec` read out of the caller's argv/envp arrays, capped to fit in one page (`E2BIG` if it doesn't).

## Process Subsystem (`kernel/src/process/`)

**`Process`** struct: PID, state, privilege (Kernel/User), base+effective priority (0–10), 16-byte name, `Box<TrapFrame>`, kernel stack, `AddressSpace`, `FileDescriptorTable`.

**Scheduler** (`process/scheduler.rs`): Multi-level priority run queue (`run_queues[0..=10]`, only Ready processes). A `wait_queue` holds Blocked and Zombie processes. One process is `running` at a time. Time slices: `BASE_QUANTUM + eff_pri * BONUS` ticks. Priority decays on preemption; periodic aging boosts starved processes. `SCHEDULER: Mutex<Scheduler>` is the global.

**Context switch** (`process/trapframe.rs`, `process/timer_preempt.rs`): The timer ISR (hand-written asm, pushes all GPRs) calls `timer_tick`. On preemption, `switch_to_next()` returns a `*const TrapFrame`; `jump_to_trapframe` restores all registers + `iretq`. The same path is used for process kill/switch.

**FPU/SSE** (`process/fpu.rs`): `Process::fpu_state` (`Box<fpu::FpuState>`, a 512-byte `#[repr(align(16))]` FXSAVE image) is saved/restored via `fxsave`/`fxrstor` at every context-switch point that also saves/restores `fs_base` (`switch_to_next`, `block_current`, `stop_and_switch_tf` save-and-restore; `kill_and_switch_tf`/`start_first` restore-only, mirroring how those two never needed `fs_base` saved either). `fpu::init()` enables SSE (`CR0.EM=0`/`MP=1`, `CR4.OSFXSR=1`/`OSXMMEXCPT=1`) and captures one real `fxsave` of the resulting clean state as the template every new `Process` starts from — must run before the first `Process` exists (wired into `init::boot()` right before `processes::init_all()`). `sys_fork` captures the parent's *live* registers with a fresh `fpu::save()` (real `fork()` semantics — the stored `Process::fpu_state` is stale as of its last preemption, not necessarily current); `sys_clone` (new thread) gets the default template instead (a fresh thread doesn't inherit register contents); `sys_exec` resets to the template, written directly to live hardware next to the `fs_base`/TLS reset since exec continues on the same CPU without an intervening switch. Verified via `fpu_test` (`userspace/c/fpu_test.c`): loads a distinctive 128-bit pattern into `xmm0` via inline asm, spins through a pure-integer loop long enough to span hundreds of real preemptions (confirmed via the `switches_total` counter below, not just elapsed time), and checks it survived intact.

**TSS** (`process/tss.rs`): Provides the `DOUBLE_FAULT_IST_INDEX` IST stack and the kernel RSP0 stack used on ring-3 → ring-0 transitions.

## Syscall Interface (`kernel/src/process/syscall.rs`)

Triggered via the `syscall` instruction (not `int 0x80` — no IDT entry involved). `process/tss.rs` wires `IA32_LSTAR` to `syscall_entry_fast` at boot. The assembly stub pushes all GPRs onto the current stack, calls the Rust dispatcher, writes the return value back into the saved RAX slot, pops, and `sysretq`/`iretq`.

Implemented syscalls (Linux-compatible numbers — see `SyscallNumber` enum for the authoritative list):

| Number | Name | Description |
|--------|------|-------------|
| 0 | `read` | Read from fd |
| 1 | `write` | Write to fd |
| 2 | `open` | Open device/file by path |
| 3 | `close` | Close fd |
| 4/5/6 | `stat`/`fstat`/`lstat` | File metadata; `lstat` genuinely doesn't follow a symlink at the final path component (real symlink support, see below) |
| 7 | `poll` | Wait for events on up to 16 fds |
| 8 | `lseek` | Reposition file offset |
| 9/11 | `mmap`/`munmap` | Anonymous memory mapping |
| 12 | `brk` | Heap break |
| 13/14/15 | `sigaction`/`sigprocmask`/`sigreturn` | POSIX signals |
| 16 | `ioctl` | TCGETS/TCSETS* (termios, `isatty()`), TIOCGWINSZ, TIOCG/SPGRP, plus the custom `FBIO_BLIT` (`0x4642_0001`) on `/dev/fb` — full-frame scaled blit for the DOOM port, see `FbBlitArgs` |
| 20 | `writev` | Vectored write |
| 22 | `pipe` | Anonymous pipe |
| 24 | `yield` | Voluntary context switch |
| 32/33 | `dup`/`dup2` | Duplicate fd (real shared-offset semantics) |
| 35 | `nanosleep` | Sleep via hrtimer |
| 39 | `getpid` | Return current PID |
| 41/42/43/46/47/49 | `socket`/`connect`/`accept`/`sendmsg`/`recvmsg`/`bind` | Socket-style IPC channels |
| 56/57 | `clone`/`fork` | Threads (shared AddressSpace+fds) / COW process fork |
| 59 | `exec` | `(path, argv, envp)` — real argc/argv/envp built onto the new stack, see `memory/elf_loader.rs::build_initial_stack` |
| 60 | `exit` | Terminate process (immediate switch) |
| 61 | `waitpid` | Real POSIX pid overloads (`>0` exact/`0` own pgid/`-1` any child/`<-1` group), `WNOHANG`/`WUNTRACED`, real exit status incl. `WIFSIGNALED` |
| 62 | `kill` | Send a signal (single pid, no process groups) |
| 72 | `fcntl` | Only `F_DUPFD`/`F_DUPFD_CLOEXEC` do something; rest are validity-checked stubs |
| 21 | `access` | `F_OK`/`R_OK`/`X_OK` just mean "resolves" (no uid/permission model); `W_OK` actually probes writability — opens the path `O_WRONLY` and issues a zero-length `write()`, since every read-only filesystem's regular-file handle unconditionally errors on `write()` regardless of length, while `RamFileHandle`'s `write()` with an empty buffer is a true no-op |
| 82/83/84/87 | `rename`/`mkdir`/`rmdir`/`unlink` | VFS mutation — ramfs (`/tmp`) and ext2 (`/mnt`) both support these (real alloc/free of blocks+inodes on ext2, see the ext2 section below); devfs/initramfs/procfs remain read-only |
| 88 | `symlink` | `(target, linkpath)` — real symlink creation on ramfs and ext2 (`Inode::symlink`, default `EROFS` elsewhere, same convention as `create`/`mkdir`); `target` is stored verbatim, unresolved, exactly like real `symlink(2)` |
| 89 | `readlink` | Real symlink target read (`fs::vfs::resolve_no_follow` + `Inode::readlink`) |
| 90/91 | `chmod`/`fchmod` | Real on ext2 (persists `i_mode`'s permission bits, see below); on every other filesystem, validity-checked stubs (path/fd must resolve) — no per-inode permission-bits storage exists there to actually change |
| 158 | `arch_prctl` | `ARCH_SET_FS` (TLS base) |
| 202 | `futex` | Wait/wake, backs mlibc mutexes/condvars |
| 213/232/233 | `epoll_create`/`epoll_wait`/`epoll_ctl` | Epoll |
| 217 | `getdents64` | Directory entries, `linux_dirent64` layout. Deliberately does NOT use `with_current_process`: that would hold the `SCHEDULER` lock across the call into `FileHandle::getdents64`, and `fs::procfs`'s live-pid listing needs a *fresh* `SCHEDULER` lock of its own (`scheduler::all_pids()`) — self-deadlocks otherwise (spin locks aren't reentrant). Same clone-the-fd-table-Arc-then-drop-the-scheduler-lock shape as `sys_read`'s generic path |
| 218 | `set_tid_address` | Stub for TLS/thread bookkeeping |
| 228 | `clock_gettime` | `CLOCK_REALTIME` is a real wall-clock reading (CMOS RTC read once at boot, see Time Subsystem below, plus uptime since); `CLOCK_MONOTONIC`/`CLOCK_BOOTTIME` are uptime, unaffected by wall-clock |
| 400/401/402 | `uptime_ms`/`uptime_sec`/`meminfo_kb` | Custom, above the Linux syscall range — debug/introspection only |
| 403 | `kdebug_ctl` | Get/set `kernel::debug`'s runtime tracing mask (get: `cmd=0`; set: `cmd=1`, subsystem name + on/off) — backs the `kdebug` userspace program |
| 404 | `statvfs` | Custom (real `statvfs(2)` has no fixed Linux syscall number of its own — glibc/mlibc implement it over `statfs`, which this port doesn't wire). One physical-memory pool backs every mount, so every path reports the same Buddy-allocator-derived total/free block counts — enough for `df` to run and show live numbers, not a real per-mount breakdown |

Helpers `with_current_process` and `with_scheduler` guarantee `cli` before lock and `sti` after lock is dropped to prevent deadlocks with the timer ISR. `sys_close`/`sys_dup2` deliberately avoid `with_current_process` (see their doc comments) — closing a handle can run a `Drop` impl that needs a fresh `SCHEDULER` lock, which would self-deadlock if the outer helper were still holding it.

## Runtime Tracing & Counters (`kernel/src/debug.rs`)

Named, independently-toggleable tracing subsystems (`MM`, `SCHED`, `FS`, `PROC`), gated by a runtime bitmask that defaults to all-off — tracepoints stay in the code permanently instead of being hand-added and stripped out per bug (which is what happened repeatedly before this module existed, and made a 2026-07-19 leak/panic investigation slow: the relevant line was buried under thousands of always-on `[COW]` lines). Add a tracepoint with `crate::ktrace!(crate::debug::MM, "...", args)` — a no-op (one relaxed atomic load + branch) when that subsystem is off. Toggle live, no rebuild: `kdebug mm on` / `kdebug mm off` (userspace program, `userspace/c/kdebug.c`, backed by syscall 403 `kdebug_ctl`). A handful of permanent counters (`forks_total`, `execs_total`, `reaps_total`, `cow_faults_resolved/failed`, `orphan_blocks_reclaimed`/`orphan_inodes_reclaimed`, `switches_total`) are always on and readable via `/proc/kdebug` (`cat /proc/kdebug`), same convention as `/proc/meminfo`. `switches_total` (full context switches since boot) exists because a per-switch `serial_println!` — the first thing tried to confirm `fpu_test` (see FPU/SSE above) was actually exercising the context-switch path — made exec()/page-fault-heavy boot phases crawl, running on literally every timer preemption; a plain atomic counter is free by comparison. The former always-on `[COW]`/`[RPU]`/`[EXEC]` debug prints in `memory/address_space.rs`, `memory/page_table_manager.rs`, and `process/syscall.rs` are now `ktrace!(MM, ...)`/`ktrace!(SCHED, ...)` calls under this module.

## Device Driver Framework (`kernel/src/drivers/`)

Drivers implement `FileHandle` (trait in `process/file.rs`): `read`, `write`, `close`, plus optional `stat`/`dup`/`getdents64` (defaults: no metadata, not dup-able, `ENOTDIR`). Device drivers are stateless (state lives in kernel globals), so their `dup()` impls just construct a fresh instance of the same type.

Register a new driver by:
1. Creating `kernel/src/drivers/<name>.rs` implementing `FileHandle`
2. Adding one entry to the `DEVICES` static slice in `drivers/mod.rs`

Current devices: `/dev/null`, `/dev/zero`, `/dev/console` (serial), `/dev/fb` (framebuffer), `/dev/kbd` (non-blocking keyboard, char/ANSI stream), `/dev/input/event0` and `/dev/input/event1` (non-blocking, wire-compatible with real Linux evdev — each `read()` returns one real `struct input_event`, 24-byte-record layout shared via `drivers/evdev.rs`). `event0` is the keyboard (`EV_KEY` + a real `linux/input-event-codes.h` `KEY_*` code + press/release value, followed by an `EV_SYN`/`SYN_REPORT`, sourced from the PS/2 IRQ's raw scancode decode — see `drivers/dev_input_event.rs`; note the underlying ring buffer fills from every keypress since boot, so a game must drain the backlog at startup, see `doom-port/doomgeneric_constanos.c::DG_Init`). `event1` is the PS/2 mouse (`EV_REL` `REL_X`/`REL_Y` for relative motion, `EV_KEY` `BTN_LEFT`/`BTN_RIGHT`/`BTN_MIDDLE` for buttons — see `mouse.rs` for the 8042 aux-device enable sequence + 3-byte packet decode, and `drivers/dev_mouse_event.rs` for the evdev translation). Both back the DOOM port's input (keyboard + mouse-look). `/dev/input/*` lives under a one-level-deep devfs subdirectory (`fs/devfs.rs::InputDirInode`) — devfs is otherwise flat, so this is a hardcoded special case, not a general nested-device mechanism. `/dev/dsp` (`drivers/dev_dsp.rs`) is a write-only, fixed-format (48000 Hz stereo s16le) PCM sink backed by the AC97 PCI driver (`ac97.rs`) — see below.

**PCI + AC97 audio** (`pci.rs`, `ac97.rs`): this kernel's only PCI-aware code — `pci.rs` does raw 0xCF8/0xCFC config-space access and a bus-0 device scan (nothing else in this kernel enumerates PCI; every other driver targets a fixed legacy ISA port). `ac97.rs` finds the Intel 82801AA AC'97 codec (`-device AC97` in QEMU), does the cold-reset + PCM-out-stream-reset + mixer-unmute sequence, and runs a **polling**, not interrupt-driven, bus-master DMA ring: the IDT is a `spin::Once`, populated once as literally the first line of `boot()` before `memory::init_core` — wiring up a PCI IRQ whose vector is only known after enumeration doesn't fit that without either an early pre-memory PCI scan or a bigger IDT refactor, so `write_pcm()` instead polls the hardware's CIV register directly and blocks (spinning, no lock held across the spin, so the timer ISR/scheduler still preempts normally) until a buffer-descriptor slot frees. The 32-entry hardware BDL aliases only 8 real physical ring buffers (`entry[i].addr = slot_phys[i % 8]`) so the hardware's native mod-32 index wraparound still works correctly without needing all 32 to be distinct allocations. Fixed format only (48000 Hz stereo s16le, AC97's native non-VRA operating point) — no `ioctl` negotiation, matching the same "one client, one format, document it" simplification `/dev/input/event0`+`event1` already use.

VFS mounts (`kernel/src/fs/mod.rs`): `/dev` (devfs), `/` (initramfs, embedded ELFs — a real two-level tree: root contains a real `bin` subdirectory, `/bin/<name>` is a genuine directory lookup, not a second mount aliasing the same flat namespace, see `fs::initramfs`), `/tmp` (ramfs, writable), `/mnt` (ext2, read-write, best-effort — see the ext2 section below), `/proc` (procfs, read-only, synthetic — `/proc/meminfo` generated fresh on every `open()` from the live Buddy allocator stats; `/proc/self` and `/proc/<pid>/exe` are real symlinks, see `fs::procfs`). `ls /` also shows every other mount (`dev`, `tmp`, `mnt`, `proc`) as an entry — `fs::vfs::direct_children` lets initramfs's root directory list them dynamically, same idea as a real Linux rootfs pre-creating empty `/proc`, `/dev`, etc. that mounts later overlay; actual traversal into them is still redirected by the mount table before ever reaching initramfs, so they only need to look like directories, not serve one.

**Filesystem: ext2 (`kernel/src/fs/ext2.rs`, mounted read-write at `/mnt`).** Real block/inode allocation (bitmap scan + free-count bookkeeping in the BGD/superblock) with direct, singly-, doubly-, and triply-indirect block addressing (~16 GiB+ files at this driver's 1024-byte block size). Supports `create`/`mkdir`/`unlink`/`rmdir`/`rename`, real symlinks (`Ext2Inode::symlink`/`readlink`, both ext2's "fast" representation — target inline in `i_block`'s own bytes, under 60 bytes, no data block allocated — and "slow" — target stored as ordinary file content, this driver writes whichever fits and reads both), and real `chmod`/`fchmod` (persists `i_mode`'s permission bits — the one filesystem here where `stat()` reports genuine per-file mode instead of a hardcoded constant). A single coarse `EXT2_LOCK` serializes every mutating op (bitmap scans aren't atomic and this kernel is preemptible); read-only paths (`lookup`/`readdir`) don't take it, since every mutating method already holds it while calling them internally and `spin::Mutex` isn't reentrant.

No journal, so a crash mid-operation can still leak an allocated-but-unlinked block/inode — every multi-step mutation orders its writes "allocate & write content, then link" so a crash can only ever leak, never dangle. Two passes at mount time (`Ext2Fs::mount()`'s callers in `init()`, before `/mnt` is exposed to the VFS) clean up after exactly that: `reconcile_free_counts` recomputes the BGD/superblock free block/inode counters from the bitmaps directly (those are separate, independently-flushed writes from what they summarize, so a crash between them drifts the counts), and `reclaim_orphans` walks every inode actually reachable from root (mirroring real `e2fsck`'s passes 1-4) and frees any block/inode the bitmaps mark used that the walk never reached.

**Critical ordering invariant in `reclaim_orphans`:** the reachability walk (`mark_reachable(ROOT_INO, ...)`) must run *before* the reserved-inode range (`1..first_ino`, which includes root's own inode 2) gets pre-marked "used" — `mark_reachable`'s own cycle guard treats an already-marked bit as "already visited, nothing more to do here." Pre-marking root first used to make the very first call return immediately without ever reading root's blocks or descending into a single child, silently treating the *entire* real directory tree as unreachable — the sweep then freed nearly every live block/inode on every fresh mount, and the next allocation handed out an already-live block to unrelated file data, corrupting whatever legitimately owned it. This is exactly what produced an `add_dir_entry` "range end index ... out of range" panic the first time this surfaced: root directory's own data block had been reused for a new file's content. Also guarded: the superblock's own block (at `first_data_block`, easy to mis-place one-off with "everything strictly before it") and sparse_super's backup superblock+BGDT copies in other block groups, both reserved unconditionally per group rather than replicating mke2fs's exact backup-placement rule (group 0, 1, and powers of 3/5/7) — reserving a slot that turns out not to have a backup costs nothing, since the real per-group bitmap never marks it used anyway.

`unlink`/`rmdir` must persist the deleted inode's zeroed record (`write_inode`) *before* clearing its bitmap bit (`free_inode`) — `free_all_blocks` only updates the in-memory copy; skipping the write-back left a stale, pre-delete record (nonzero mode, dangling block pointers into blocks the bitmap already shows free) that a real `e2fsck` flags as a disconnected inode. `i_dtime` (deletion timestamp) is stamped with a real Unix epoch (`crate::time::now_unix_secs()`) — a raw boot-relative uptime value there is small enough to collide with a different on-disk use of that same field (ext3+ threads its in-progress orphan-inode list through `i_dtime` as a next-inode-number link), which `e2fsck` misdiagnoses as a corrupted orphan chain purely because the value looks too small to be a real calendar time.

## Time Subsystem (`kernel/src/time/`, `kernel/src/rtc.rs`)

Monotonic time (`time::clocksource`, TSC-backed when available, jiffies fallback) is unrelated to wall-clock time, which this kernel gets from a real CMOS/MC146818 RTC (`rtc.rs`, ports `0x70`/`0x71`) read exactly once at boot (`time::init()`, before `fs::init()` mounts ext2 — dtime stamps need it available already). `time::now_unix_secs()` = that one boot-time reading + monotonic uptime since; there's no periodic RTC IRQ and none is needed for this. `rtc::read_unix_time()` handles BCD-vs-binary and 12-vs-24-hour format (Status Register B), the standard double-read-until-stable technique to avoid a snapshot torn across the chip's once-a-second update window, and an exact integer year/month/day → Unix-epoch conversion (Howard Hinnant's `days_from_civil`, correct across the full Gregorian leap-year rule, no floating point). Best-effort like every other optional hardware probe here (mouse, AC97): if the RTC never settles, `now_unix_secs()` just degrades to reporting uptime (boot = epoch), same as before this existed. No century register (unreliable across BIOS/QEMU configs) — assumes 2000-2099.

`/proc` enumerates every live pid for real (`scheduler::all_pids()`, walking `running` + every run queue + the wait queue) — `ls /proc`/`opendir("/proc")` see them all, not just pids looked up by exact name (previously the only way in). Each `/proc/<pid>/stat` renders the classic Linux `stat` format (`fn render_proc_stat`) from a live `Process` snapshot — this is what backs BusyBox `ps`/`top`.

**Real symlinks** (`fs/vfs.rs`): `Inode::readlink()`, `resolve()` (follows a symlink at every path component including the final one — `open`/`stat` semantics) vs `resolve_no_follow()` (leaf left alone — `lstat`/`readlink` semantics), both with an 8-hop `ELOOP` guard. `fs::procfs` produces synthetic ones (`/proc/self`, `/proc/<pid>/exe`); ramfs (`/tmp`) supports creating *real* ones via the `symlink()` syscall (`Inode::symlink`, only writable filesystem that implements it — same `EROFS`-by-default convention as `create`/`mkdir`). This is what backs PID 1's real `busybox --install -s /tmp/bin` at boot (see Userspace Programs below) — no synthetic, kernel-computed symlinks anywhere anymore; `/tmp/bin/<applet>` are indistinguishable from symlinks a real Linux install would create.

**Permission bits** (`fs::types::Stat`): no real per-inode permission model — `regular()` (initramfs/ext2/procfs) hardcodes `0o444`, `regular_writable()` (ramfs only) hardcodes `0o644`. Added because BusyBox `vi`'s readonly check is `access(fn, W_OK) < 0 || !(st_mode & (S_IWUSR|...))` — fixing `access()` alone wasn't enough; every regular file reported zero write bits regardless of which filesystem it actually lived on, so `vi` opened `/tmp/*` files `[Readonly]` too.

The `FileDescriptorTable` per process holds up to 16 open files. FD 0 (stdin) is pre-opened to `/dev/console` (serial — real reads still come from the shared keyboard/UART ring buffer regardless of the handle here); FDs 1/2 (stdout/stderr) are both pre-opened to `/dev/fb` so user-process output and errors are visible on the actual screen, not just in `serial.log` — `FramebufferConsole::write` mirrors every byte it renders out over COM1 too (`[fb] ` prefix), so headless/serial-log debugging still sees everything.

## Userspace Programs (`kernel/src/process/user_programs.rs`)

Embedded ELF binaries live in `kernel/embedded/`, rebuilt automatically by `kernel/build.rs` (three families, each built differently):

- **Rust** (`RUST_PROGRAMS` in `build.rs`) — built via `cargo build --release` in `userspace/` (separate Cargo workspace), copied from `userspace/target/x86_64-unknown-none/release/`.
- **C** (`C_PROGRAMS` in `build.rs`) — `userspace/c/<name>.c`, compiled directly with `clang` against `sysroot/` (built by `scripts/setup-mlibc.sh` if missing).
- **BusyBox** (`BUSYBOX_ELF` in `build.rs`) — external `make`-based build via `scripts/build-busybox.sh` (git submodule at `busybox/`, config at `busybox-config/minimal.config`), only invoked when `kernel/embedded/busybox.elf` is missing (unlike the two families above, this isn't rebuilt unconditionally — it's slow and `make` already does its own incremental rebuilds). **Caveat of "only if missing":** after any change to sysroot ABI headers (`mlibc-port/.../abi-bits/*.h`), `rm kernel/embedded/busybox.elf` (and `doom.elf`) so the constants don't stay baked into the old static binary — this is exactly how the `SEEK_SET=3` bug survived one rebuild cycle.
- **DOOM** (`DOOM_ELF` in `build.rs`) — doomgeneric (git submodule `doomgeneric/`) + our platform port `doom-port/doomgeneric_constanos.c`, built by `scripts/build-doom.sh`; rebuilt when `doom.elf` is missing *or* the port file is newer than it (mtime check — the port file is the only input that changes in practice). The Freedoom IWAD is downloaded by `scripts/fetch-freedoom.sh` into `disk-image-root/`, from where the workspace-root `build.rs` seeds it into `disk.img` (ext2), and DOOM reads it at runtime from `/mnt/freedoom1.wad` — an earlier version routed it through a kernel-embedded `/dev/freedoom1.wad` device instead, worked around what looked like ATA read corruption under DOOM's access pattern that turned out to be the SEEK_SET ABI bug below; gone now that that's fixed. Video: `/dev/fb`'s custom `FBIO_BLIT` ioctl (userspace hands a `0x00RRGGBB` buffer + dims; kernel nearest-neighbor scales and letterboxes it — `Framebuffer::blit_scaled`); raw-blit clients bypass the text console's cursor tracking entirely, so `FBIO_BLIT` flags the framebuffer dirty and the console does one full clear + cursor reset on its next text write (otherwise the next shell prompt draws over DOOM's last frame — see `drivers/framebuffer_console.rs`'s `FB_RAW_DIRTY`). Input: `/dev/input/event0` (keyboard) + `/dev/input/event1` (PS/2 mouse, real evdev wire format, see Device Driver Framework above) — `DG_DrawFrame` accumulates a frame's worth of `EV_REL` deltas and posts one `event_t{type=ev_mouse}` via `D_PostEvent`, giving real mouse-look (turn on X, forward/back on Y, `BT_ATTACK` on left click); PS/2's own sign convention (X+ = right, Y+ = up/away from the user) already matches what `g_game.c`'s mouse handling expects, so deltas are passed through unnegated. Audio: sound effects only (no music — this doomgeneric fork ships no MIDI/OPL synthesis backend at all, unrelated to the driver work) via `doom-port/doomgeneric_sound_constanos.c`'s `sound_module_t DG_sound_module` — decodes DMX sfx lumps (8-bit unsigned PCM, `W_CacheLumpNum`/`W_GetNumForName`, doomgeneric's own portable WAD API), mixes up to 16 channels with 16.16-fixed-point nearest-neighbor resampling up to 48000 Hz stereo, and writes the mixed buffer to `/dev/dsp` once per `Update()` call (~35/sec). `i_sound.c` unconditionally `#include <SDL_mixer.h>` and references `DG_music_module`/`use_libsamplerate`/`libsamplerate_scale` whenever `FEATURE_SOUND` is defined (upstream assumes an SDL_mixer-based backend) — satisfied with an empty `doom-port/stub-include/SDL_mixer.h` (no `Mix_*` symbol is actually used) and a no-op `DG_music_module` in the same sound port file, rather than patching the doomgeneric submodule itself. Run it by typing `doom` in ash.
- **Quake** (`QUAKE_ELF` in `build.rs`) — [`erysdren/quakegeneric`](https://github.com/erysdren/quakegeneric) (git submodule `quakegeneric/`, a doomgeneric-style minimal port of id Software's GPL WinQuake source) + our platform port `quake-port/quakegeneric_constanos.c`, built by `scripts/build-quake.sh`; same "rebuilt if missing or the port file is newer" staleness check as DOOM. quakegeneric's own README claims "32-bit only" — verified that's overly conservative for a straight compile (built clean at `-m64` on the host with only two harmless warnings, neither a real pointer-width bug) before committing to the port. The shareware `id1/pak0.pak` is downloaded by `scripts/fetch-quake-shareware.sh` (an archive.org mirror of the original `quake_pak.zip`, extracting only the freely-redistributable shareware `pak0.pak`, never the full-game `pak1.pak` also in that archive) into `disk-image-root/id1/`, seeded into `disk.img` (ext2) the same way Freedoom is — `disk.img` is 96MiB, not 48MiB, to fit both IWADs plus headroom. Video: same `FBIO_BLIT` ioctl as DOOM, but quakegeneric hands `QG_DrawFrame` an 8bpp *paletted* 320x240 buffer (not ready-to-blit RGB like doomgeneric), so the port does its own index→RGB conversion via the palette `QG_SetPalette` last supplied. Input: same `/dev/input/event0`+`event1` real evdev devices as DOOM, but pulled (`QG_GetKey`/`QG_GetMouseMove`, called from inside the engine's own frame processing) rather than pushed like DOOM's `D_PostEvent` model — the port drains both fds into small queues/counters once per outer-loop iteration. Audio: sound effects via `quake-port/quakegeneric_sound_constanos.c` (replaces upstream's silent `snd_null.c`), reusing `/dev/dsp`/`ac97.rs` the same way DOOM's sound port does — reimplements the engine's `S_*` API directly (`S_Init`/`S_StartSound`/`S_Update`/channel mixing) rather than hooking a real mixing library, since quakegeneric has no `snd_mixer`-equivalent of its own. Caches decode WAV lumps straight out of `pak0.pak` (`COM_LoadTempFile`) via a small hand-rolled RIFF/WAVE chunk parser — the shareware set is 8-bit mono PCM only, verified directly against the pak, so anything else is treated as "no sound" rather than mixed wrong — then resamples to 48000 Hz stereo s16le with the same 16.16 fixed-point nearest-neighbor technique `doomgeneric_sound_constanos.c` already uses, mixing up to 32 channels. Needed two Quake-side memory fixes once real WAV data started flowing: `quakegeneric.c`'s hardcoded 8 MiB `parms.memsize` (bumped to 64 MiB via an idempotent submodule patch in `scripts/build-quake.sh`, same "patch via build script, never the checkout" convention as mlibc's `do_scanf` patch) and `Z_Malloc`'s separate, much smaller 48 KiB zone heap (`zone.c`'s `DYNAMIC_SIZE`, distinct from the general hunk — raised via `-zone 8192` in the default argv `quakegeneric_constanos.c`'s `main()` injects). The WAV parser also had a real out-of-bounds read: an unrecognized/malformed chunk's declared size went negative once cast to a signed `int` for advancing the parse cursor, sending it wildly out of bounds and reading adjacent heap memory as if it were more WAV chunks — fixed by bounds-checking each chunk's declared size against remaining file length *before* trusting it for anything, not just clamping the final `data` chunk's length after the fact. Verified via `QEMU_AUDIODEV="wav,id=snd0,path=..."` capture + `ffmpeg -af volumedetect` (real signal, not silence). Run it by typing `quake` in ash.

**Growable user stack** (`memory::vma::VmaKind::GrowableStack`, `VmaList::grow_stack`, `elf_loader::STACK_PAGES`/`STACK_MAX_PAGES`): every process's stack VMA starts at 64 KiB and the page fault handler (`find_vma_fast_or_grow` in `process::scheduler`, wired into `init::devices::page_fault_handler`'s VMA-lookup step) extends it downward on demand — up to 8 MiB, an `RLIMIT_STACK`-style cap — when a fault lands within a guard gap just below the current low boundary. No program needs its real stack usage known in advance; this replaced an earlier hardcoded per-program override (added for Quake, whose `Host_Init` call chain overflows a small fixed stack) that required guessing every future program's needs by name. **Known flaky pre-existing bug, unrelated to this mechanism:** `busybox --install`'s own `fork()` hangs or double-faults roughly 1 boot in 3-4, reproducible on the unmodified codebase with no Quake/stack changes at all — see the `busybox_install_fork_flake` memory. (An early diagnosis wrongly pinned this on a stack-size change; it isn't — the same failure rate holds with `STACK_PAGES` left completely untouched.)

Only the embedded/*.elf → `PROGRAMS` registration step is manual:

1. Write the program (Rust in `userspace/src/bin/`, C in `userspace/c/`) or point at an externally-built ELF.
2. Register it in the relevant `build.rs` list (skip this for BusyBox-style external builds).
3. Add `("name", ProgramSource::Elf(include_bytes!("../../embedded/name.elf")))` to `PROGRAMS` in `user_programs.rs` — this alone makes it runnable both via `sys_exec`/the shell (any typed command not matching a shell builtin falls through to `fork()`+`exec_argv()`) and visible under `/bin` in initramfs (`ls`, `opendir`).

Only `shell` is spawned automatically at boot (`init/processes.rs`, still looked up by that literal name) — it's PID 1, a minimal init loop, not an interactive shell itself. Before ever touching `ash`, it runs `install_busybox_symlinks()`: `mkdir("/tmp/bin")` then a real `fork()`+`exec()` of `busybox --install -s /tmp/bin` (`waitpid()`-ed to completion) — genuine `symlink(2)` calls, one per applet BusyBox was actually compiled with, using BusyBox's own `--install` machinery (`CONFIG_BUSYBOX` + `CONFIG_FEATURE_INSTALLER`), not anything this kernel computes or hand-maintains. `PATH=/tmp/bin:/bin` is then passed to `ash` so plain-name lookups find them. Only after that does the main loop start: `fork()`+exec `busybox ash`, `waitpid()`, and respawn `ash` if it ever exits (its own `exit`, Ctrl-D, or a crash) instead of leaving the system with no way to type anything — see `userspace/src/bin/shell.rs::_start`. Real BusyBox `ash` (job control, line editing, `FEATURE_SH_STANDALONE`+`FEATURE_SH_NOFORK` applet dispatch, see `busybox-config/minimal.config`) is the only interactive shell now; everything else is launched on demand from it.

BusyBox's applet set now covers real day-to-day use, not just a smoke test: `vi` (full-screen editor — needed the framebuffer console's `ESC[J` no-param case, real `TIOCGWINSZ` dimensions instead of a hardcoded 80×25, `CONFIG_FEATURE_VI_WIN_RESIZE` enabled — without it `query_screen_dimensions()` is a compiled-out no-op and `vi` never even calls the ioctl, silently sticking to its built-in 24×80 fallback regardless of how correct `TIOCGWINSZ` is — and the `access()`/`W_OK` + `Stat::regular_writable` fixes below to stop opening every file `[Readonly]`), `grep`/`sed`/`awk`/`find`/`sort`/`diff`/`xargs`, `tar`/`gzip`/`gunzip`, `ps`/`top` (via the real `/proc` pid enumeration above), `df` (via `statvfs`), `du`, `chmod`, `id`/`hostname` (see mlibc sysdeps below), `md5sum`, `od`/`hexdump`, `less`/`more`. `free` is the one applet deliberately left out — `procps/free.c` gates `sysinfo()` behind `#ifdef __linux__`, which this cross-compile target doesn't define; spoofing that macro risks changing behavior under every *other* `#ifdef __linux__` in BusyBox's ~250K lines, a blast radius far bigger than one applet is worth.

`sys_exec` (`process/syscall.rs`) resolves the requested path through the **real VFS**, not a special-cased table lookup: `resolve_exec_path` cwd-normalizes the path, then manually walks symlinks (`fs::vfs::resolve_no_follow` + `Inode::readlink`, up to 8 hops, `ELOOP` beyond that) to a canonical absolute path, which is then `fs::vfs::open()`'d and read fully into an owned buffer for the ELF loader — no more flat `PROGRAMS`-table-only fast path. This is what makes `/bin/hello` (a real `$PATH` search candidate), `./ls` (explicit relative path), a bare `hello`, and `/proc/self/exe` (a real symlink, see below) all resolve through one uniform mechanism instead of three different ones agreeing by coincidence. The canonical resolved path is recorded as `Process::exe_name` (inherited across `fork()`/`clone()`) — this is what `/proc/<pid>/exe` reports.

The fallback `ProgramSource::RawCode` embeds inline assembly tests from `process/user_test_fileio.rs` and is used for bootstrapping when no ELF exists.

## mlibc Port (`mlibc-port/constanos-sysdeps/`)

`scripts/setup-mlibc.sh` copies this into the `mlibc/` submodule checkout and rebuilds `sysroot/` — it's the only thing that survives a `git submodule update` reset of `mlibc/` itself, so **any fix that needs to live inside the `mlibc/` submodule tree goes through a patch step in `setup-mlibc.sh`, never a direct edit to the checkout** (see the `do_scanf` patch below for the pattern: idempotency-checked via `grep`, then a Python string-replace, with an explicit error if upstream's text ever stops matching).

**ABI-constant hygiene:** the `abi-bits/*.h` headers were originally copied from non-Linux mlibc ports and have repeatedly disagreed with this kernel's Linux-numbered syscall ABI (`MAP_ANONYMOUS`, `O_CREAT`, `POLLOUT`, `F_DUPFD`, `WIFEXITED`, `ENOTEMPTY`, and most recently `SEEK_SET`, which was `3` — `lseek(fd, n, SEEK_SET)` returned EINVAL while SEEK_CUR/SEEK_END coincidentally worked, making files "go empty" after any `fseek(END)` size probe). When touching any of these headers, cross-check values against `mlibc/abis/linux/` and the kernel's own constants, rebuild the sysroot, **and delete `kernel/embedded/busybox.elf` + `doom.elf`** so the "only build if missing" binaries don't keep the old constants baked in.

**Real upstream mlibc bug, patched here:** `options/ansi/generic/stdio.cpp`'s `do_scanf` only advanced its internal `count` inside the `if(typed_dest)` branch of the `append_to_buffer` lambda shared by the `%s`/`%c`/`%[` conversions. A *suppressed* conversion (`%*s` — `dest` deliberately null) never touched `count`, so the very next `NOMATCH_CHECK(count == 0)` read "matched nothing" regardless of what was actually consumed, and `do_scanf` returned early right at the first `%*s` in any format string — silently truncating the match count for everything after it. Found via BusyBox `ps`/`top`: `libbb/procps.c`'s `/proc/<pid>/stat` parser skips half its fields with exactly that conversion, so every pid was read correctly but `procps_scan` still reported zero matches (`n=5` instead of the required `11`). Not specific to this port or to BusyBox — any `sscanf`/`fscanf` call with a `%*s` anywhere in it was affected.

Sysdeps added beyond the original bootstrap set (all in `generic/generic.cpp` unless noted): `sys_access`, `sys_symlink`/`sys_symlinkat`, `sys_chmod`/`sys_fchmod`/`sys_fchmodat`, `sys_statvfs`/`sys_fstatvfs`, `sys_getgroups`. A few aren't real kernel round-trips at all: `sys_getuid`/`geteuid`/`getgid`/`getegid` return `0` unconditionally (single-user kernel — matches the existing style), `uname()`/`gethostname()`/`sethostname()` are plain userspace stubs (hostname is a per-process static, so `sethostname` in one process is invisible to a process started afterward — nothing here needs cross-process persistence), and `setmntent`/`getmntent`/`endmntent` (`mntent.h`) port the kernel's own fixed, compile-time mount table directly rather than parsing a real (nonexistent) `/etc/mtab` — enough for `df` with no arguments to enumerate mounts. `sysinfo()` (backs `free`, blocked — see BusyBox note above) is a real implementation, not a stub, built from two syscalls this port already had: `SYS_statvfs` for total/free bytes and `SYS_uptime_sec` for uptime; its header (`include/sys/sysinfo.h`) is a standalone port of mlibc's own Linux-option-only version, since enabling that whole option was ruled out for the same reason as the `free` applet itself.

## Key Design Invariants

- **Buddy is the only physical frame allocator** after `init_core`. Do not create a second `BootInfoFrameAllocator` over the same memory regions.
- **`memory` module does NOT import `process`**. Demand paging is kept dependency-free from the process layer; the fault handler in `init/devices.rs` bridges them.
- **Interrupt safety:** Always `cli` before acquiring `SCHEDULER` and `sti` after releasing it. The timer ISR acquires the lock; holding it with interrupts enabled causes a deadlock.
- **Context switches restore all GPRs** via `jump_to_trapframe` (asm `pop` sequence + `iretq`). Never use partial restores that leave callee registers from the killed process.
