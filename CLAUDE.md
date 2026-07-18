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

The top-level `cargo run` builds the kernel ELF (via artifact dependency), wraps it in a UEFI disk image via the `bootloader` crate, then spawns `qemu-system-x86_64`. Serial output appears in the terminal.

There is no test framework wired up; `cargo test` is not used.

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
| 4/5/6 | `stat`/`fstat`/`lstat` | File metadata (`lstat` aliases `stat` — no symlinks yet) |
| 7 | `poll` | Wait for events on up to 16 fds |
| 8 | `lseek` | Reposition file offset |
| 9/11 | `mmap`/`munmap` | Anonymous memory mapping |
| 12 | `brk` | Heap break |
| 13/14/15 | `sigaction`/`sigprocmask`/`sigreturn` | POSIX signals |
| 16 | `ioctl` | Only enough for `isatty()` (TCGETS on fd 0-2) |
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
| 61 | `waitpid` | Reap a specific child pid (no `-1`/"any child"; exit status always reported as 0 — see `sys_waitpid`'s doc comment for why that's not trivial to fix) |
| 62 | `kill` | Send a signal (single pid, no process groups) |
| 72 | `fcntl` | Only `F_DUPFD`/`F_DUPFD_CLOEXEC` do something; rest are validity-checked stubs |
| 82/83/84/87 | `rename`/`mkdir`/`rmdir`/`unlink` | VFS mutation — only ramfs (`/tmp`) supports these, ext2/devfs/initramfs are read-only |
| 158 | `arch_prctl` | `ARCH_SET_FS` (TLS base) |
| 202 | `futex` | Wait/wake, backs mlibc mutexes/condvars |
| 213/232/233 | `epoll_create`/`epoll_wait`/`epoll_ctl` | Epoll |
| 217 | `getdents64` | Directory entries, `linux_dirent64` layout |
| 218 | `set_tid_address` | Stub for TLS/thread bookkeeping |
| 228 | `clock_gettime` | |
| 400/401/402 | `uptime_ms`/`uptime_sec`/`meminfo_kb` | Custom, above the Linux syscall range — debug/introspection only |

Helpers `with_current_process` and `with_scheduler` guarantee `cli` before lock and `sti` after lock is dropped to prevent deadlocks with the timer ISR. `sys_close`/`sys_dup2` deliberately avoid `with_current_process` (see their doc comments) — closing a handle can run a `Drop` impl that needs a fresh `SCHEDULER` lock, which would self-deadlock if the outer helper were still holding it.

## Device Driver Framework (`kernel/src/drivers/`)

Drivers implement `FileHandle` (trait in `process/file.rs`): `read`, `write`, `close`, plus optional `stat`/`dup`/`getdents64` (defaults: no metadata, not dup-able, `ENOTDIR`). Device drivers are stateless (state lives in kernel globals), so their `dup()` impls just construct a fresh instance of the same type.

Register a new driver by:
1. Creating `kernel/src/drivers/<name>.rs` implementing `FileHandle`
2. Adding one entry to the `DEVICES` static slice in `drivers/mod.rs`

Current devices: `/dev/null`, `/dev/zero`, `/dev/console` (serial), `/dev/fb` (framebuffer), `/dev/kbd` (non-blocking keyboard).

The `FileDescriptorTable` per process holds up to 16 open files. FDs 0/1/2 are pre-opened to `/dev/console` (stdin/stdout/stderr).

## Userspace Programs (`kernel/src/process/user_programs.rs`)

Embedded ELF binaries live in `kernel/embedded/`, rebuilt automatically by `kernel/build.rs` (three families, each built differently):

- **Rust** (`RUST_PROGRAMS` in `build.rs`) — built via `cargo build --release` in `userspace/` (separate Cargo workspace), copied from `userspace/target/x86_64-unknown-none/release/`.
- **C** (`C_PROGRAMS` in `build.rs`) — `userspace/c/<name>.c`, compiled directly with `clang` against `sysroot/` (built by `scripts/setup-mlibc.sh` if missing).
- **BusyBox** (`BUSYBOX_ELF` in `build.rs`) — external `make`-based build via `scripts/build-busybox.sh` (git submodule at `busybox/`, config at `busybox-config/minimal.config`), only invoked when `kernel/embedded/busybox.elf` is missing (unlike the two families above, this isn't rebuilt unconditionally — it's slow and `make` already does its own incremental rebuilds).

Only the embedded/*.elf → `PROGRAMS` registration step is manual:

1. Write the program (Rust in `userspace/src/bin/`, C in `userspace/c/`) or point at an externally-built ELF.
2. Register it in the relevant `build.rs` list (skip this for BusyBox-style external builds).
3. Add `("name", ProgramSource::Elf(include_bytes!("../../embedded/name.elf")))` to `PROGRAMS` in `user_programs.rs` — this alone makes it runnable both via `sys_exec`/the shell (any typed command not matching a shell builtin falls through to `fork()`+`exec_argv()`) and visible in `/` under initramfs (`ls`, `opendir`).

Only `shell` is spawned automatically at boot (`init/processes.rs`); everything else is launched on demand from the shell.

The fallback `ProgramSource::RawCode` embeds inline assembly tests from `process/user_test_fileio.rs` and is used for bootstrapping when no ELF exists.

## Key Design Invariants

- **Buddy is the only physical frame allocator** after `init_core`. Do not create a second `BootInfoFrameAllocator` over the same memory regions.
- **`memory` module does NOT import `process`**. Demand paging is kept dependency-free from the process layer; the fault handler in `init/devices.rs` bridges them.
- **Interrupt safety:** Always `cli` before acquiring `SCHEDULER` and `sti` after releasing it. The timer ISR acquires the lock; holding it with interrupts enabled causes a deadlock.
- **Context switches restore all GPRs** via `jump_to_trapframe` (asm `pop` sequence + `iretq`). Never use partial restores that leave callee registers from the killed process.
