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
1. `devices::init_idt()` — load IDT (exceptions, PIC IRQs, syscall at `int 0x80`)
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

**ELF loader** (`memory/elf_loader.rs`): Parses ELF64 PT_LOAD segments, maps them into a fresh `AddressSpace`, zeros BSS, and registers demand-paged stack. Static executables only (no dynamic linker).

## Process Subsystem (`kernel/src/process/`)

**`Process`** struct: PID, state, privilege (Kernel/User), base+effective priority (0–10), 16-byte name, `Box<TrapFrame>`, kernel stack, `AddressSpace`, `FileDescriptorTable`.

**Scheduler** (`process/scheduler.rs`): Multi-level priority run queue (`run_queues[0..=10]`, only Ready processes). A `wait_queue` holds Blocked and Zombie processes. One process is `running` at a time. Time slices: `BASE_QUANTUM + eff_pri * BONUS` ticks. Priority decays on preemption; periodic aging boosts starved processes. `SCHEDULER: Mutex<Scheduler>` is the global.

**Context switch** (`process/trapframe.rs`, `process/timer_preempt.rs`): The timer ISR (hand-written asm, pushes all GPRs) calls `timer_tick`. On preemption, `switch_to_next()` returns a `*const TrapFrame`; `jump_to_trapframe` restores all registers + `iretq`. The same path is used for process kill/switch.

**TSS** (`process/tss.rs`): Provides the `DOUBLE_FAULT_IST_INDEX` IST stack and the kernel RSP0 stack used on ring-3 → ring-0 transitions.

## Syscall Interface (`kernel/src/process/syscall.rs`)

Triggered via `int 0x80`. The assembly stub (`syscall_entry`) pushes all GPRs onto the current stack, calls `syscall_handler_asm`, writes the return value back into the saved RAX slot, pops, and `iretq`.

Implemented syscalls (Linux-compatible numbers):

| Number | Name | Description |
|--------|------|-------------|
| 0 | `read` | Read from fd |
| 1 | `write` | Write to fd |
| 2 | `open` | Open device by path |
| 3 | `close` | Close fd |
| 24 | `yield` | Voluntary context switch |
| 39 | `getpid` | Return current PID |
| 60 | `exit` | Terminate process (immediate switch) |

Helpers `with_current_process` and `with_scheduler` guarantee `cli` before lock and `sti` after lock is dropped to prevent deadlocks with the timer ISR.

## Device Driver Framework (`kernel/src/drivers/`)

Drivers implement `FileHandle` (trait in `process/file.rs`): `read`, `write`, `close`.

Register a new driver by:
1. Creating `kernel/src/drivers/<name>.rs` implementing `FileHandle`
2. Adding one entry to the `DEVICES` static slice in `drivers/mod.rs`

Current devices: `/dev/null`, `/dev/zero`, `/dev/console` (serial), `/dev/fb` (framebuffer).

The `FileDescriptorTable` per process holds up to 16 open files. FDs 0/1/2 are pre-opened to `/dev/console` (stdin/stdout/stderr).

## Userspace Programs (`kernel/src/process/user_programs.rs`)

Embedded ELF binaries live in `kernel/embedded/`. Workflow to add a new program:

1. Write it as a standalone `#![no_std]` binary targeting `x86_64-unknown-none`
2. Build it and copy the ELF to `kernel/embedded/<name>.elf`
3. Add `("name", ProgramSource::Elf(include_bytes!("../../embedded/name.elf")))` to `PROGRAMS` in `user_programs.rs`
4. Wire it up in `init/processes.rs`

The fallback `ProgramSource::RawCode` embeds inline assembly tests from `process/user_test_fileio.rs` and is used for bootstrapping when no ELF exists.

## Key Design Invariants

- **Buddy is the only physical frame allocator** after `init_core`. Do not create a second `BootInfoFrameAllocator` over the same memory regions.
- **`memory` module does NOT import `process`**. Demand paging is kept dependency-free from the process layer; the fault handler in `init/devices.rs` bridges them.
- **Interrupt safety:** Always `cli` before acquiring `SCHEDULER` and `sti` after releasing it. The timer ISR acquires the lock; holding it with interrupts enabled causes a deadlock.
- **Context switches restore all GPRs** via `jump_to_trapframe` (asm `pop` sequence + `iretq`). Never use partial restores that leave callee registers from the killed process.
