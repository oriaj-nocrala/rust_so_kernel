# CPUs: per-CPU init, SMP bring-up, interrupts, TLB shootdown, time, sensors

Code: `kernel/src/cpu/`, `kernel/src/smp.rs`, `kernel/src/interrupts/`, `kernel/src/memory/tlb.rs`, `kernel/src/time/`, `kernel/src/rtc.rs`. Pure parts: `hal::{apic,smp,tlb,cpufreq,k10temp,amd_power}`. Plan and history: `docs/smp/smp-plan.md`. The scheduler side of SMP is in `processes-and-scheduling.md`.

## Per-CPU init (`cpu::init_this_cpu(cpu)`)

- **The one place per-CPU state is set up**, for the BSP and for every AP:
  - CR0/CR4/EFER bits copied from the BSP;
  - the GDT with this CPU's TSS slot (`0x28 + 16·n`), each TSS with its own double-fault IST stack;
  - `lidt`;
  - `PerCpu`/`KERNEL_GS_BASE`;
  - the syscall MSRs;
  - `IA32_PAT`;
  - SSE;
  - LAPIC and its timer.
- Each step has a `verify_*` that reads the register back. Results: `cpu_init:` in `/proc/kdebug`.
- Global *decisions* stay elsewhere (`program_pat`, `apic::init`); this function only *applies* them.
- **Anything new that is per CPU gets a step here and a `verify_*`**, or the APs will run with the firmware's value.
- Test: `hw_tests::init_this_cpu_restores_what_an_ap_lacks`.
- `cpu::cpu_id()` reads the task register (`str`), not `gs:`.

## AP bring-up (`smp.rs`)

- INIT-SIPI-SIPI through a 4-page trampoline below 640 KiB, **reserved in `init_core` before the buddy is seeded**. Its PML4 is a copy of the kernel's plus a 0–2 MiB identity map.
- `ap_entry` loads the real CR3, runs `init_this_cpu`, joins the TLB shootdown (`this_cpu_ready`), then waits in `sti; hlt` until `release_aps` (at the first process's start).
- **An AP's LAPIC timer stays masked until it is released**: before that it has no process to switch from.
- APs start one at a time with bounded waits. One that doesn't answer is logged `NO-RESPONSE(stage n)`, sent INIT again, and left out.
- The BSP is CPU 0; the rest follow MADT order, up to `MAX_CPUS` = 32.
- `smp::run_on` + `WAKE_VECTOR` (0xF1) run a job on an idle AP (used by the TLB self-test).
- Observe: `smp:` in `/proc/kdebug`. QEMU: `QEMU_DEBUG_SMP=N`.

## Interrupt controllers (`interrupts/`, `hal::apic`)

- The tick is the **LAPIC timer** (100 Hz, calibrated against the TSC, which was calibrated against the PIT), used as a **one-shot clock event per CPU** (`interrupts::apic`): every timer interrupt re-arms for the earlier of this CPU's next 100 Hz tick (`NEXT_TICK_NS`) and the earliest `hrtimer` (`hrtimer::next_expiry`), and `hrtimer::start` brings the interrupt forward when its expiry comes first (`hrtimer_started`). Only an interrupt at a tick boundary is a *tick* (`apic::tick_due`: time slices, CPU-time accounting, `TICK_COUNT`, the BSP's polling); an earlier one just drains the hrtimers (any CPU drains them). So a timeout fires at its own time: `nanosleep(200 us)` takes ~220 us on KVM (it took a whole tick, 10 ms, when the timer was periodic). The PIT fallback (APIC declined) keeps the plain 100 Hz tick. `timer_test` pins both the resolution and the 100 Hz rate. ISA IRQs (keyboard 1, COM1 4, mouse 12) go through the **I/O APIC**, with the MADT overrides applied. The 8259 stays masked.
- `apic::init` is best-effort: if anything fails, the 8259 + PIT keep working (`-cpu max,-apic` tests this). Uses x2APIC only if the firmware already enabled it.
- Vectors: timer 32, ISA line n → 32+n, TLB shootdown 0xF0, wake 0xF1, reschedule 0xF2, LAPIC spurious 0xFF (never EOI'd).
- **Drivers call `interrupts::enable_isa_irq(line)` and `interrupts::eoi(vector)`, never `pic::*`.**
- Unhandled vectors 34–47 check the LAPIC ISR to decide whether to EOI.
- Edge-triggered lines are drained when switching controllers.
- The IDT is a `spin::Once` built at the start of boot. That is why PCI devices (xHCI, AC97) are polled instead of using interrupts.
- Observe in `/proc/kdebug`: `irq_controller:` and `timer_ticks: N over M ms` (compare two readings).

## TLB shootdown (`memory/tlb.rs`)

- After any PTE change, every *other* CPU that may cache the entry gets an IPI (0xF0), and the sender waits:
  - user mapping → the CPUs whose CR3 is that table right now: `tlb::invalidate_page(pml4, addr)`;
  - kernel mapping → every ready CPU: `invalidate_kernel_page`.
- Who has which table loaded is `LOADED[cpu]`, published before the CR3 write. **So every CR3 load goes through `tlb::switch_to`.**
- One request at a time. The wait is bounded: 1 s, then a panic naming the CPUs.
- **A CPU spinning with IF=0 can't take the IPI**, so every such spin calls `tlb::service_pending`. Already covered:
  - `IrqMutex`/`sync::Mutex`/`IrqLock` relax;
  - vfs locks (relax hook);
  - USB transfer waits.
- The COW path swaps frames with `replace_frame` (a single PTE store).
- Tests: `hw_tests::tlb_shootdown_leaves_no_stale_translation`, `kdebug tlbtest`. Observe: `tlb:` in `/proc/kdebug`.
- **Known issue (QEMU/TCG, 4 CPUs; not seen on the Ryzen):** under concurrent fork/exec and cold file reads the 1 s ack wait can expire (`never acknowledged`, `unmap_kernel_guard_page` in `fork_impl` or `remap_kernel_guard_page` in `try_free_kernel_stack` on the sender). `scripts/tlb-stress.sh` reproduces it (6/6 runs); `scripts/run-abi-suite.sh gui_comp_test` hits it about one run in three. Seen with gdb (a live panic, no autorun): two CPUs in `shoot` at once, the one that holds `SENDER` waits for the other, which is in the `SENDER` loop calling `service_pending`, and the ack only lands later (PENDING was clear when looked at afterwards): a CPU that does not run for over a second of guest time, which points at the emulator's scheduling of the vCPU rather than at the protocol; not proven. Do not read a panic of this shape in QEMU as a regression of whatever was being tested: run `scripts/tlb-stress.sh` on the unchanged tree first.

## Time

- Monotonic time: `time::clocksource` (TSC, with a jiffies fallback). hrtimers run on CPU 0.
- Wall clock: `time::now_unix_secs()` = the CMOS RTC read once at boot (`time::init`, before `fs::init`) + uptime.
  - `rtc.rs` handles BCD, 12h mode and torn reads, and assumes the years 2000–2099.
  - If the RTC never settles, boot time = the epoch.

## Frequency, temperature, power (AMD Zen, bare metal only; QEMU shows none of it)

- **`cpu MHz`** in `/proc/cpuinfo` = base × ΔAPERF/ΔMPERF.
  - Each CPU reads its own MSRs in its tick (`hal::cpufreq::Window`, `try_with`); a value needs 10 ms of C0 time.
  - Gated on CPUID 6.ECX[0]; `flags` shows `aperfmperf` when present. Without it, the TSC frequency is reported.
- **Temperature** (`/proc/sensors`, Linux k10temp logic): SMN reads through 00:00.0 config 0x60/0x64 (`pci::smn_read`). Tctl at 0x59800, plus one Tccd per CCD.
  - Needs an AMD vendor, family 17h/19h, *and* an AMD root complex. It claims 00:18.3.
  - Line format: `chip<TAB>type<TAB>label<TAB>value` (e.g. `k10temp temp Tctl 32875`, millidegrees; `amd_energy energy Esocket0 <µJ>`). Empty without hardware support.
- **Idle**: `hlt` by default. `kdebug idle c2` switches to ACPI C2 via an I/O read of `CStateBaseAddr+1`; measured, it saves nothing over `hlt`.
- `/proc/kdebug`: `rapl:` (package energy) and `c0_permille:` (C0 residency per CPU). Read temperatures only after the machine has settled.
