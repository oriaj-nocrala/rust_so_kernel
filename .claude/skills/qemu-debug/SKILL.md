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
scripts/qemu-debug.sh log 50                    # serial.log; also rawlog, dlog (-d int trace, TCG only)
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
| `QEMU_DEBUG_NO_DISK=1` / `NO_AC97=1` / `NO_PS2=1` / `NO_USB=1` | remove the ext2 disk / AC97 / 8042 / xHCI |
| `QEMU_DEBUG_DISK_IF=ide` | attach `disk.img` to IDE (the ATA PIO driver, VirtualBox's shape) instead of virtio-blk; ATA is ~6x slower to mount under KVM |
| `QEMU_USB_KBD=1`, `QEMU_USB_MOUSE=1`, `QEMU_USB_STORAGE=<img>` | USB devices |
| `QEMU_DEBUG_EXTRA_ARGS` | anything else (e.g. `-no-reboot`, `-cpu max,-apic`) |
| `QEMU_AUDIODEV=wav,id=snd0,path=x.wav` | capture audio |
| `QEMU_GDB_PORT` | gdbstub port (default 1234) |

## Reproduce Ryzen bugs here first

Iterating on metal costs a physical reboot per try. Make QEMU look like the Ryzen, **one variable at a time**: `QEMU_DEBUG_MEM=8G`, `QEMU_DEBUG_NO_DISK=1`, `QEMU_DEBUG_NO_PS2=1 QEMU_USB_KBD=1`, `QEMU_DEBUG_NO_AC97=1`, `QEMU_USB_STORAGE=`. A bug that looked hardware-specific (TSC, no PS/2) was reproduced on the first try with `-m 768M`. QEMU does **not** model: the cpufreq/temperature/RAPL MSRs, write-combining, VRAM slowness, or torn xHCI TRB writes.

## Intermittent failures

`scripts/boot-matrix.sh N M` runs N QEMUs in parallel, M boots each (one qcow2 overlay per instance), and classifies every boot as `OK`/`HANG`/`PANIC`/`DOUBLE_FAULT` with `cpus_online=M/N`. Serial logs of non-OK boots are kept; read them.

## KVM and timing

- `qemu-debug.sh` (and `run-kernel-tests.sh`, `cargo run`) use KVM whenever `/dev/kvm` is usable: syscalls ~0.56 us, a pipe round trip ~30 us. `QEMU_ACCEL=tcg` forces software emulation, ~30x slower (~10 us, ~900 us): only for `-d int` traces (`dlog` is empty under KVM), a CPU feature KVM's host lacks, or a bug that needs a slow guest to show. Never quote TCG numbers as performance. `scripts/run-abi-suite.sh latency_bench` prints wake-up latencies.
- Where a test or program spends its time: `kdebug sysprof on`, run it, `cat /proc/sysprof` (calls, wall time and slowest call per syscall per program, and the call each live process is in right now). `run-abi-suite.sh` does this for every test (`kernel-testing` skill).

## gdb

- `start --gdb` adds a gdbstub without stopping the CPU; attach whenever needed (e.g. once it hangs). `--gdb-freeze` stops at the reset vector.
- `scripts/qemu-debug.sh gdb "info registers" "bt" "p \$rip"` runs `rust-gdb`/`gdb` in batch mode against the most recently built kernel ELF (never stripped).
- The kernel is a PIE loaded at a runtime offset. The script greps `virtual_address_offset:` from serial.log and uses `add-symbol-file -o`, so symbols resolve.
- A gdbstub can also be added to a running QEMU from its monitor (`gdbserver`).

## Tracing and counters (`kernel/src/debug.rs`, crate `diag`)

- **Tracepoints**: `crate::ktrace!(crate::debug::MM, "fmt", args)`. Subsystems: `MM`, `SCHED`, `FS`, `PROC`. All off by default; an off tracepoint costs one relaxed load. Toggle live from the shell: `kdebug mm on|off` (syscall 403). Add tracepoints permanently.
- **serial.log is kept quiet on purpose** (it is read every session): per-process chatter is tracepoints, not `serial_println!`. Fork, exec (`sys_exec: loading`, the `ELF:` segment lines), page-table creation, scheduler queueing and plain exits are under `kdebug proc on` / `kdebug sched on` / `kdebug mm on`; the 24-word raw user stack dump at a segfault is under `mm`. What stays: deaths by a signal or fault (`💀 Killed PID`), the one-line fault report and the symbolizable `user backtrace:`. A new routine per-process message goes in a tracepoint too.
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

## Live panic or hang, with every CPU (what cracked the 2026-10-01 cases)

- `qemu-debug.sh` runs **one CPU by default** and the target is SMP: use `QEMU_DEBUG_SMP=4` for anything with fork/exec or blocking I/O. Some hangs exist only at one CPU (the ext2 block-cache lock), some panics only at four.
- **Boot without an autorun job** for a live post-mortem: with a job, a panic resets the machine and `-no-reboot` makes QEMU exit, so the state is gone. Type the workload with `send`/`enter`, wait for `KERNEL PANIC` in `serial.log`, then:
  `echo "gdbserver tcp::1234" | socat - UNIX-CONNECT:$STATE/monitor.sock` and `QEMU_DEBUG_STATE_DIR=$STATE scripts/qemu-debug.sh gdb "thread apply all bt 12"`; `echo "info registers -a" | socat - UNIX-CONNECT:$STATE/monitor.sock` shows each CPU's RIP, IF (RFL) and TR.
- A job that "never finishes" may be slow, not stuck: take the snapshot twice and compare a loop variable (an advancing LBA was progress). Do not call a TIMEOUT a hang.
- **Inject input into a headless run** through the monitor socket directly: `mouse_move 20 10`, `sendkey a`, `mouse_button 1`/`mouse_button 0` (the `qemu-debug.sh mouse-move/key` subcommands answered "Not running" with a custom `QEMU_DEBUG_STATE_DIR`). Wait for a line the *job* prints at its start, not one the program prints at its end.
- Parallel runs: one `QEMU_DEBUG_STATE_DIR` each (short path) and a copy of `disk.img` (`scripts/tlb-stress.sh`). **Never `pkill -f` a pattern that appears in your own command line** (it kills the shell running it).
- **A `TLB shootdown ... never acknowledged` panic is a real bug, not an emulator flake**: some long IF=0 section does not call `tlb::service_pending` (`docs/reference/cpu.md`, fixed once in e0fd174). Find the section (gdb on the paused guest) instead of re-running until it passes; `scripts/tlb-stress.sh` reproduces the class.
