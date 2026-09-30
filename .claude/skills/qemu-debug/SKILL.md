---
name: qemu-debug
description: Playbook for debugging this kernel (rust_so_kernel) in QEMU without a display or keyboard: booting headless, typing into the shell, reading serial.log and screendumps, attaching gdb with the PIE offset, measuring intermittent boot failures with boot-matrix, reproducing real-hardware (Ryzen) bugs in QEMU, and adding/toggling ktrace tracepoints and /proc/kdebug counters. Use whenever you need to run the kernel to observe behaviour, chase a hang/panic/crash, or instrument a subsystem. Keywords: qemu-debug.sh, boot-matrix, gdb, serial.log, sendkey, screendump, ktrace, kdebug, /proc/kdebug, hang, panic, double fault.
---

# Debugging in QEMU

## Rules

- **Always use `scripts/qemu-debug.sh`**, never a hand-built `qemu-system-x86_64` command line or your own key-sending script. The full subcommand list is in its header comment.
- Wait on a condition (`wait-for`), never on a blind `sleep`.
- **Never print on every context switch or timer tick**: the I/O cost looks exactly like a deadlock. Add a counter instead (see Tracing).
- **Keep instruments, don't delete them**: a diagnostic built for one bug goes into `kernel::debug`/`diag` for good.
- **Measure the instrument before trusting it**: run the identical workload several times on unchanged code. A difference smaller than that run-to-run spread is not a finding.
- **Read the preserved serial log before believing a failure verdict**; harnesses have misclassified boots.
- Program output written to the screen is also in serial.log (prefix `[fb] `). ANSI codes stay readable there as text (`rawlog` shows escape bytes as `^[`), so a `grep` usually beats a screendump.

## Session

```bash
scripts/qemu-debug.sh start [--no-build] [--release] [--gdb] [--gdb-freeze]
scripts/qemu-debug.sh wait-for "About to start first process" [TIMEOUT]
scripts/qemu-debug.sh send "ls /mnt/bin" && scripts/qemu-debug.sh enter
scripts/qemu-debug.sh key ctrl-c                # raw QEMU key names
scripts/qemu-debug.sh mouse-move 10 -5          # also: mouse-button 1|2|4|0
scripts/qemu-debug.sh log 50                    # serial.log; also rawlog, dlog (-d int trace)
scripts/qemu-debug.sh screendump out.png        # then Read the PNG
scripts/qemu-debug.sh stop
```

- Keystrokes go through the monitor's `sendkey`, paced so the PS/2 ISR doesn't drop them. With `QEMU_USB_KBD=1` they arrive through xHCI instead.
- **Two sessions at once**: give each its own `QEMU_DEBUG_STATE_DIR` (keep the path short: the socket path must be under 108 bytes) and a scratch disk: `cp disk.img /tmp/x.img; QEMU_DEBUG_DISK_IMG=/tmp/x.img`. Two QEMUs writing the real `disk.img` can corrupt it.

## Environment knobs

| Variable | Effect |
|----------|--------|
| `QEMU_DEBUG_MEM=8G` | RAM (default 512M; every real machine has more) |
| `QEMU_DEBUG_SMP=N` | CPUs |
| `QEMU_DEBUG_NO_DISK=1` / `NO_AC97=1` / `NO_PS2=1` / `NO_USB=1` | remove the ATA disk / AC97 / 8042 / xHCI |
| `QEMU_USB_KBD=1`, `QEMU_USB_MOUSE=1`, `QEMU_USB_STORAGE=<img>` | USB devices |
| `QEMU_DEBUG_EXTRA_ARGS` | anything else (e.g. `-no-reboot`, `-cpu max,-apic`) |
| `QEMU_AUDIODEV=wav,id=snd0,path=x.wav` | capture audio |
| `QEMU_GDB_PORT` | gdbstub port (default 1234) |

## Reproduce Ryzen bugs here first

Iterating on metal costs a physical reboot per try. Make QEMU look like the Ryzen, **one variable at a time**: `QEMU_DEBUG_MEM=8G`, `QEMU_DEBUG_NO_DISK=1`, `QEMU_DEBUG_NO_PS2=1 QEMU_USB_KBD=1`, `QEMU_DEBUG_NO_AC97=1`, `QEMU_USB_STORAGE=`. A bug that looked hardware-specific (TSC, no PS/2) was reproduced on the first try with `-m 768M`. QEMU does **not** model: the cpufreq/temperature/RAPL MSRs, write-combining, VRAM slowness, or torn xHCI TRB writes.

## Intermittent failures

`scripts/boot-matrix.sh N M` runs N QEMUs in parallel, M boots each (one qcow2 overlay per instance), and classifies every boot as `OK`/`HANG`/`PANIC`/`DOUBLE_FAULT` with `cpus_online=M/N`. Serial logs of non-OK boots are kept; read them.

## KVM and timing

- `qemu-debug.sh` runs QEMU in software emulation (TCG) unless told otherwise: syscalls cost ~10 us and a pipe round trip ~900 us, ~30x KVM's (`QEMU_DEBUG_EXTRA_ARGS="-enable-kvm"`: 0.56 us and ~30 us). Measure latency and throughput with KVM, and run the suite both ways: KVM's speed exposes tests that assume a slow guest (spin counts). `scripts/run-abi-suite.sh latency_bench` prints wake-up latencies.

## gdb

- `start --gdb` adds a gdbstub without stopping the CPU; attach whenever needed (e.g. once it hangs). `--gdb-freeze` stops at the reset vector.
- `scripts/qemu-debug.sh gdb "info registers" "bt" "p \$rip"` runs `rust-gdb`/`gdb` in batch mode against the most recently built kernel ELF (never stripped).
- The kernel is a PIE loaded at a runtime offset. The script greps `virtual_address_offset:` from serial.log and uses `add-symbol-file -o`, so symbols resolve.
- A gdbstub can also be added to a running QEMU from its monitor (`gdbserver`).

## Tracing and counters (`kernel/src/debug.rs`, crate `diag`)

- **Tracepoints**: `crate::ktrace!(crate::debug::MM, "fmt", args)`. Subsystems: `MM`, `SCHED`, `FS`, `PROC`. All off by default; an off tracepoint costs one relaxed load. Toggle live from the shell: `kdebug mm on|off` (syscall 403). Add tracepoints permanently.
- **Counters**: atomics shown in `/proc/kdebug` (`forks_total`, `switches_total`, `cow_faults_*`, `early_wakes`, `waits_interrupted`, USB, cache, TLB, sched, irq, cpu_init, smp, …). New counter: add it to `kernel::debug` and to the report.
- **Always-on lock diagnostics** in `/proc/kdebug` and the panic snapshot:
  - `LockDiag` (`SCHEDULER_LOCK`): acquires, releases, last acquirer's `file:line`;
  - `DirLockDiag` (ramfs): the same, keyed by pid + operation;
  - `TfRewindDiag` (`TF_REWIND`): a process resumed from a stale `TrapFrame`. It prints when it fires, which should never happen.
- A detector that is only valid under a particular test harness must be removed together with that harness.
- Other: `/proc/dmesg` (klog ring), `kdebug panic` (test the panic path), `kdebug tlbtest`, `kdebug sync`.
- A user process killed by a fault prints a red `kalert!` line plus a user stack dump on serial (`init::devices::dump_user_stack`).

## GUI

`scripts/gui-e2e.sh [term|wm|text]` drives the compositor through the monitor and checks screendumps pixel by pixel; the checks are listed in its header.
