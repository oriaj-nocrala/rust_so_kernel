# CLAUDE.md

constanos: an x86-64 hobby kernel in Rust (`no_std`, UEFI, SMP), with a Linux-numbered syscall ABI, mlibc as libc, BusyBox as userland, and a small Wayland-style compositor. It runs in QEMU and on one real machine (AM4/Ryzen, no serial port, USB-only input and storage).

## Build and run

Needs Rust **nightly** (`rust-toolchain.toml`), `qemu-system-x86_64`, `e2fsprogs`, and clang for the C programs.

```bash
cargo run                                              # build everything + boot in QEMU (serial on stdout)
cargo build                                            # kernel ELF + UEFI image + disk.img
cd kernel && cargo build --target x86_64-unknown-none  # the kernel crate alone
```

- The root `build.rs` builds the kernel with a **nested** `cargo build` (not `bindeps`, which panics with `-Z build-std`) and wraps it in a UEFI image. `kernel/build.rs` builds and strips every userspace program (see the `userspace-programs` skill).
- **A root `cargo build` does not compile `hal`/`ext2`/`mm`/`vfs`/`diag`/`sched`/`usock`/`tty`/`nvgpu`/`net`**: the root `build.rs` doesn't watch them, so cargo skips the nested build and exits 0. Verify them with `cd kernel && cargo build --target x86_64-unknown-none` (or `touch build.rs`).
- `disk.img` (ext2, mounted at `/mnt`) is created once and then kept. Each build syncs `disk-image-root/` into it with `debugfs`. To regenerate it: delete it and `touch build.rs`.

## Skills (load the one that matches the task)

| Skill | When |
|-------|------|
| `kernel-testing` | Before calling any change done: which suite covers which crate, QEMU integration tests, proving tests by sabotage |
| `qemu-debug` | Running the kernel headless, typing into it, gdb, boot-matrix, reproducing Ryzen bugs, `ktrace!`/`/proc/kdebug` |
| `metal-run` | Anything on the Ryzen: deploying to the USB stick, unattended jobs, reading the log partition |
| `userspace-programs` | Adding a program, `kernel/build.rs` lists, `disk.img`, mlibc and BusyBox changes |
| `gpu-display` | Display work on the NVIDIA GA106 (`nvgpu`, `kernel/src/gpu/`): recipe (oracle → fixture → pure code → replay test → sabotage → adapter → `gpu=` level → job), code map, nouveau reference map |
| `gpu-gsp` | GSP-RM on the GA106 and phase 6: `nvgpu::{falcon,fwsec,firmware,gspmem,booter,rpc,rm}`, `kernel/src/gpu/gsp.rs` (`gpu=fwsec`/`gpu=gsp`), GPU page tables `nvgpu::mmu`, GPFIFO channel + copy engine `nvgpu::chan` (`gpu=vaspace`/`gpu=copy`): code map, VRAM/VA memory map, bring-up ladder, doorbell/token, where NVIDIA's hardware manuals are (`~/src/gpu-ref/open-gpu-doc`), mutation-testing tool, metal stability protocol, phase 6d measurements/rules (BAR1 after GSP-RM, link ceiling, CE interrupt, RC events) |
| `linux-abi` | Making Linux/musl/Rust-std binaries run: running a std program on the kernel, testing a syscall with a raw C test proven by sabotage, the traps (opt-level-0 stack copies, IF=0, fork masking PTE bugs), what is still missing |
| `gpu-g5` | Many GPU clients, sharing buffers/timelines between processes, the GPU lock discipline, Mesa import/export, `vk_share`, `gpu-multi.sh`, and where the WSI (layer 3) starts |
| `kernel-drivers` | Writing or porting a driver (`hal` seams, `/dev` entries, driver tests) |

## Code map

| Path | What | Details |
|------|------|---------|
| `kernel/src/init/` | boot sequence (below), fault handlers (`devices.rs`) | |
| `kernel/src/memory/`, `allocator/` + crate `mm` | buddy/slab, page tables, VMAs, COW, shm, TLB | `docs/reference/memory.md` |
| `kernel/src/process/` + crate `sched` | processes, scheduler, context switch, signals, waits, CPU time | `docs/reference/processes-and-scheduling.md` |
| `kernel/src/process/syscall/` | syscall entry and table | `docs/reference/syscalls.md` |
| `kernel/src/ipc/` + crates `usock`, `tty` | pipes, AF_UNIX, ptys | `docs/reference/ipc.md` |
| `kernel/src/fs/`, `block/` + crates `vfs`, `ext2` | VFS, mounts, procfs, ext2, block devices | `docs/reference/filesystems.md` |
| `kernel/src/drivers/`, `pci.rs`, `ac97.rs` | `/dev` files, PCI, audio | `docs/reference/drivers.md` |
| `kernel/src/framebuffer.rs`, `drivers/framebuffer_console.rs` + crates `gui`, `vt`, `draw`, `text`, `img` | framebuffer, console, `/dev/fb0`, compositor, GUI libraries, PNG + alpha blits | `docs/reference/graphics.md` |
| `kernel/src/gpu/`, `interrupts/msi.rs`, `memory/dma.rs`, `firmware.rs`, `bootopts.rs` + crate `nvgpu` | NVIDIA GA106 driver (behind `gpu=`, off by default), MSI vectors, DMA buffers, firmware loading, boot options (`/mnt/etc/kernel.conf`) | `docs/reference/gpu.md`, plan `docs/gpu/gpu-plan.md` |
| `kernel/src/network/` + crate `net`, `hal/src/virtio.rs` | virtio-net driver (polled), smoltcp stack, DHCP, AF_INET UDP, TCP and raw ICMP sockets | `docs/reference/net.md`, plan `docs/net/net-plan.md` |
| `kernel/src/usb/` | xHCI keyboard, mouse, mass storage | `docs/reference/usb.md` |
| `kernel/src/cpu/`, `smp.rs`, `interrupts/`, `time/` | per-CPU init, APs, APIC, TLB shootdown, time, sensors | `docs/reference/cpu.md` |
| `kernel/src/klog.rs`, `autorun.rs`, `watchdog.rs`, `block/logpart.rs` | kernel log, log partition, unattended runs | `docs/reference/metal.md` |
| `userspace/`, `mlibc-port/`, `busybox-config/`, `*-port/` | programs, libc, BusyBox, DOOM/Quake | `docs/reference/userspace.md` |
| `kernel/src/debug.rs` + crate `diag` | tracing, counters, lock diagnostics, `IrqMutex` | `qemu-debug` skill |
| `hal/` | hardware seams (`PortIo`/`PhysMem`/`BlockDevice`) + pure driver logic | `kernel-drivers` skill |

- **Only `so2` (the root crate: build script + QEMU launcher) and `kernel` form the Cargo workspace.** Every other crate is its own workspace, so its `cargo test` can unwind (the root profile is `panic = "abort"`), and is pulled in by `path` dependency.
- They exist because **`kernel` can't run `cargo test`**: logic that can be written against plain types moves into a crate and gets host tests.
- The pattern for those crates: **nothing blocks, and effects come back as data** (wakeups, signals, events). The kernel adapter owns the globals and does the blocking.
- Design docs and plans (the *why* and *what next*): `docs/` (index `docs/README.md`), especially `docs/smp/smp-plan.md`, `docs/gui/gui-plan.md` and `docs/drivers/`.

## Boot sequence (`kernel/src/init/mod.rs`, `init::boot`)

1. IDT (a `spin::Once`, built first, which is why PCI devices are polled); vfs relax hook and clock.
2. Framebuffer and font; `memory::init_core` (physmap, AP trampoline reserved, buddy seeded, COW table); allocator smoke test.
3. `program_pat`, `map_write_combining`, `attach_shadow` (framebuffer); `watchdog::arm_early`.
4. ACPI; boot screen; PIC + PIT; PS/2 mouse; AC97 (best effort, bounded waits).
5. TSC calibration, cpufreq/temp/idle probes; `apic::init` (LAPIC timer + I/O APIC, falls back to 8259 + PIT).
6. `cpu::init_this_cpu(0)`; `smp::start_aps()` (the APs park in `hlt`).
7. `time::init` (RTC); USB; `fs::init` (mounts `/mnt` from USB, else ATA); `autorun::detect`; `watchdog::settle`; `logpart::init`.
8. `fpu::init` (FXSAVE template, needed before the first process); `processes::init_all` (one idle process per CPU + PID 1).
9. `start_first_process`: release the APs, enable interrupts, jump to PID 1.

PID 1 (`userspace/src/bin/shell.rs`) runs `busybox --install -s /tmp/bin`, then keeps a `busybox ash` alive with `PATH=/tmp/bin:/bin:/mnt/bin`. In autorun mode it runs the job instead.

## Key design invariants

Breaking one of these has cost days of debugging each time. The *why* is kept short on purpose; more in the code comments and `docs/hang-hunt-bug2-findings.md`.

**Memory**
- The buddy allocator is the **only** physical frame allocator after `init_core`.
- The `memory` module does **not** import `process`. The fault handler in `init/devices.rs` bridges them.
- Kernel code that writes another process's memory uses `AddressSpace::copy_to_user`/`copy_from_user`/`prepare_user_write`, never `translate_page` + a physmap write (the frame may be the zero frame or a shared COW frame).

**Interrupts and locks**
- **Any lock also taken by an ISR, or taken on an allocating path, is a `diag::IrqMutex` or `IrqLock`** (they disable interrupts before locking and restore the previous state afterwards), or the ISR uses `try_lock` (`FB_STATE`, `FRAMEBUFFER`, `CONTROLLERS`).
  - `BUDDY`, `SLAB_ALLOCATOR`, `SOCKETS`, `PTYS` and the address-space lock are this kind.
  - Why: the timer ISR can allocate and can take the scheduler lock. A plain lock held with IF=1 deadlocks one CPU against itself.
- **`SCHEDULER` (a `sync::Mutex`) is only taken with IF=0.** Use `SchedGuard::lock()`/`with_scheduler`/`with_current_process`, which `cli` first and `sti` after releasing. The `sti` is unconditional, so from code that already runs with IF=0 (ISRs, `sys_exit`) call `scheduler::local_scheduler()` directly.
- Kernel locks are `crate::sync::Mutex`, **never `spin::Mutex`**: its spin loop answers TLB shootdowns.
- Any other busy-wait with IF=0 calls `memory::tlb::service_pending`.
- **IF=0 is not mutual exclusion** on SMP; shared state needs a real lock.
- No new `static mut` or global `UnsafeCell` for shared state. Per-CPU state is indexed by `cpu::cpu_id()`.
- Lock order: scheduler → address space → `BUDDY`/`SLAB_ALLOCATOR`.
- Registry locks (pipes, `SOCKETS`, `PTYS`, poll waiters) come **before** `SCHEDULER`: never take one while holding the scheduler lock.
- **A process's fd-table lock also comes before `SCHEDULER`**: `sys_read`/`sys_write` hold it across `FileHandle::read/write`, which take the scheduler, and threads share one table. Never `proc.files.lock()` inside `with_current_process`/under `local_scheduler()`: use `syscall::with_files`/`with_fd_table` (clone the `Arc`, release the scheduler, then lock; IF stays 0). Breaking it deadlocked all CPUs under a multi-threaded tokio run (`fdlock_test`).
- Never drop a file handle under `SCHEDULER` (its `Drop` may take it).
- Full per-lock audit: stage 6 of `docs/smp/smp-plan.md`.

**Entry, context switch, per-CPU**
- **Every kernel entry clears DF.** `IA32_FMASK` masks it for `syscall`; the hand-written stubs (`timer_interrupt_entry`, `syscall_entry_fast`) start with `cld`; rustc's `x86-interrupt` shims do it themselves. **A new hand-written entry stub must `cld`.** `jump_to_trapframe` is a resume, not an entry, and must not clear DF. (With DF=1, `rep movsb` copied trapframes backwards.)
- Context switches restore **all** GPRs (`jump_to_trapframe`: pops + `iretq`).
- **Nothing with a live `Drop` may be in scope when calling a `-> !` function** (`jump_to_user`, `jump_to_trapframe`, block-and-switch): it never returns, so the `Drop` never runs (a leaked fd-table `Arc` once meant EOF never arrived).
- **`sys_exit` keeps IF=0 all the way to the `iretq`**: it runs on a kernel stack already queued to be freed. `scheduler::tick(rsp)` never frees the stack it interrupted, and `LEAVING[cpu]` keeps other CPUs off it.
- **`gs` is used only in `syscall_entry_fast`'s `swapgs` window.** Don't add any other `gs:` access. `percpu::check_gs_invariant` panics if `KERNEL_GS_BASE` is wrong.
- **Nothing per-process lives in a per-CPU global across a preemption point.** The syscall frame is `syscall::current_tf_ptr()`, derived from the kernel stack.
- **A process can migrate at any preemption point**: with IF=1, don't keep a `cpu_id()` (or anything indexed by it) across one.
- New per-CPU hardware state goes into `cpu::init_this_cpu`, with a `verify_*`.
- Anything new on the timer tick must state whether it is global work (CPU 0) or per-CPU work. The timer is one-shot: an interrupt is a tick only where `apic::tick_due` says so; put per-tick work in that branch of `timer_preempt_handler`, not before it.

**TLB and address spaces**
- Every PTE change invalidates through `memory::tlb` (`invalidate_page(pml4, addr)` / `invalidate_kernel_page`). Never `x86_64::instructions::tlb::*` or `MapperFlush::flush()`: call `.ignore()` and pass the page.
- Every CR3 load goes through `tlb::switch_to`.
- Never drop the last `Arc<AddressSpace>` of a table some CPU has loaded in CR3.
- Never drop one that may be the last **under `SCHEDULER`** either (it frees every page: seconds for a big process, with every CPU waiting): `process::dead_files::release_space`. `/proc/kdebug` `space_frees_under_lock` must stay 0.

**Blocking**
- **Check-then-sleep is one step.** Mechanisms: FUTEX_WAIT holds the scheduler lock; poll and stdin register and block under it; pipes and sockets use `wake_pending`/`WAKE_EPOCH`. A new blocking path needs one of these.
- A waker **claims the waiter's `WaitCell`** before touching the waiter (`processes-and-scheduling.md`).
- A syscall blocks by rewinding `rip -= 2` and parking. If it rewound, it must really block.

**Userspace ABI**
- The syscall ABI is Linux-numbered, and mlibc's `abi-bits` headers must match Linux; mismatches have caused many bugs. After changing them, rebuild the static binaries (`userspace-programs` skill).

## Keeping these docs useful

- This file holds only what every task needs. Subsystem details go in `docs/reference/<area>.md`; procedures ("how to do X") go in a skill; the reasoning behind a design goes in a `docs/` design doc.
- Write for the next LLM session: short bullets, file paths, rules stated as rules, the reason in one clause. No changelog ("until 2026-09-25 …"): history belongs in git and in the commit message.
- Update the matching doc in the same change as the code. Delete what is no longer true.
