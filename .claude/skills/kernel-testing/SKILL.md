---
name: kernel-testing
description: Playbook for verifying a change to rust_so_kernel and for writing tests: which test suite covers which crate, why a root cargo build proves nothing for the extracted crates, running and adding QEMU integration tests (#[test_case] in kernel/src/hw_tests.rs via scripts/run-kernel-tests.sh), userspace C test programs, writing tests that run fast (wait for a condition with userspace/c/testutil.h, never "sleep long enough"), reading the suite's report (per-test time, /proc/sysprof syscall profile, user backtraces), the A/B/C concurrency-test taxonomy and proving a test by sabotage. Use before declaring any change done, when adding or speeding up tests, or when a subagent reports test results. Keywords: cargo test, run-kernel-tests.sh, run-abi-suite.sh, hw_tests, test_case, host tests, sabotage, verify, testutil.h, tu_wait_blocked, sysprof, slow test, flaky test.
---

# Testing and verifying a change

## Which command verifies what

| Changed | Verify with |
|---------|-------------|
| a host-testable crate (`hal`, `ext2`, `mm`, `vfs`, `diag`, `sched`, `usock`, `tty`, `gui`, `vt`, `draw`, `text`, `img`) | `cd <crate> && cargo test` |
| a crate the kernel links (`hal` … `tty`) | **also** `cd kernel && cargo build --target x86_64-unknown-none`; a root `cargo build` skips them |
| kernel code on a hardware path | `scripts/run-kernel-tests.sh` |
| syscalls, process or FS behaviour | the matching `userspace/c/*_test.c`, run in a booted kernel (`qemu-debug` skill). The tests live in `/mnt/bin` |
| boot stability, races | `scripts/boot-matrix.sh N M` |
| the C userspace tests (all of them, one boot) | `scripts/run-abi-suite.sh [--no-build] [--keep-going] [test ...]`: ~25 s for the standard 52 under KVM; **fails fast** (first `FAIL`, kernel panic, a test over `TEST_TIMEOUT`=90 s, no output for `STALL`=60 s) and names the test; `KEEP_ALIVE=1` leaves a hung guest running; the list is `disk-image-root/abi-suite.sh`. Ends with a report (below: "Reading the suite's report") |
| real Rust std / tokio programs | `scripts/run-std-probe.sh` (`probes/std/g1_std.rs`: hard links, statx, `Command` with uid/gid), `scripts/run-tokio-probe.sh` (`probes/tokio`); both need `rustc` run from inside the repo (pinned nightly with the musl std) |
| the compositor or GUI programs | `scripts/gui-e2e.sh [term\|wm\|text]` |
| anything the Ryzen does differently | the `metal-run` skill |
| a clean clone still builds, boots and packs | CI (`.github/workflows/ci.yml`, on push to master and PRs): every host crate's `cargo test`, `cargo build`, `run-kernel-tests.sh`, `make-release-image.sh` (artifact `constanos-image`). The ABI suite runs non-blocking, only to catch the forked-child stall (`STRANDED READY`); make it blocking once that is fixed. Not the Vulkan programs (Mesa is built by hand). Try a change to it in `ubuntu:24.04` with `scripts/ci-deps.sh` first |

- `text` needs `scripts/fetch-fonts.sh` once.
- `ext2` has a known intermittent temp-file flake (`docs/fs/ext2-test-flake.md`).
- `gui/tests/c_wire.rs` needs the host `cc`.
- New logic that can be written against plain types belongs in one of the host-testable crates, with host tests.

## Rules for tests

- **A test never seen to fail is decoration.** Prove each new test by sabotage: break the invariant in a copy of the crate (use a **short** `/tmp` path, not the repo) and show that the test fails or hangs. `cargo-mutants` is not installed, and it can't express "reorder two statements", which is the shape of most invariants here. For the `nvgpu` crate use `scripts/gpu-mutate.py FILE FILTER MUTATIONS.py` (one `cargo test` per mutation, restores the file, lists survivors; examples in `nvgpu/mutations/`).
- **Label concurrency tests by family** in their doc comment:
  - **A — reentrancy probes**: no threads, deterministic; the same thread re-enters a non-reentrant lock, and a hang *is* the signal.
  - **B — contention probes** with `std::thread`: make contention observable so instruments can be validated.
  - **C — instrument audit**: the sabotage proof itself.
- **Re-run numbers yourself.** Counts and "all checks pass" in a subagent's report have been wrong every time they were checked. Re-run the suite, and grep any docs it edited for the numbers it wrote.
- Don't write test counts into docs; they go stale.

## QEMU integration tests

`scripts/run-kernel-tests.sh` builds the test kernel (`cargo build --tests` in `kernel/`), boots it headless with `-smp 4` and `isa-debug-exit`, and exits 0 only if every `#[test_case]` passed. Nonzero means a failure, a hang or a crash.

**Every QEMU launcher uses KVM when `/dev/kvm` is usable** (`qemu-debug.sh`, `qemu-test-runner`, `cargo run`, like `make-release-image.sh`). `QEMU_ACCEL=tcg` forces the emulator, for the rare case that needs it: `-d int` traces (`debug.log` is empty under KVM), a CPU feature the host lacks, or a timing-shaped bug that only shows when everything is 30x slower. TCG numbers are not performance numbers.

- **Plain `cargo test --target x86_64-unknown-none` does not work**: it builds the bin target twice, and with `-Z build-std` that gives two `core` crates (`E0152 duplicate lang item`). This script is the replacement, and it is also the target's `runner`.
- Guest side: `kernel/src/test_framework.rs`, `kernel/src/hw_tests.rs` (every case, with a doc comment), `kernel/src/init/test_support.rs::boot_for_tests` (the part of the real boot the tests need, including APIC and APs). Host side: `qemu-test-runner/`.

### Adding a case

1. Add `#[test_case] fn name()` to `kernel/src/hw_tests.rs`. Its doc comment says what it covers and why a host test can't cover it.
2. If it needs more of the boot running, extend `boot_for_tests`.
3. Prefer RAM stand-ins for devices (`hal::block::MemDisk`, a RAM-backed `Framebuffer` with `stride > width` and a pre-filled buffer), so the test can also check that nothing outside its target was touched.
4. Sabotage it once and watch it fail.

## Userspace test programs

- `userspace/c/<thing>_test.c`, in `DISK_C_PROGRAMS` (see the `userspace-programs` skill). Each one prints its cases and a pass/fail summary. Run it in a booted kernel and grep serial.log. Waits go through `testutil.h` ("Writing tests that run fast" below).
- Existing ones cover sockets, ptys, shm, pipes, signals and waits, sessions, CPU time, RSS, timestamps, FPU and input polling. Look for the matching test before writing a new one.

## Traps found while doing G5 (each cost time)

- **`scripts/run-abi-suite.sh` runs the previous binary when a C test fails to build.** The build error scrolls past above `matched: #`; if a new check does not appear in `/tmp/qemu-debug-rust_so_kernel/serial.log`, run `cargo build 2>&1 | grep error -A4` first. A new name that clashes with libc (`bind`, `send`, ...) is the usual cause.
- **A file staged in `disk-image-root/bin/` needs `touch build.rs`** before `cargo build` copies it into `disk.img`; check with `debugfs -R "ls -l /bin" disk.img`. And `dumpe2fs -h disk.img | grep Free`: 15 MB Vulkan programs filled the old 160 MiB image (now 288 MiB).
- **Sabotage a kernel change through a QEMU test in one command:** `scripts/gpu-mutate-qemu.py LIST.py` (a python file with `TEST` and `MUTS = [(file, name, old, new)]`; example `nvgpu/mutations/sharing_kernel.py`). It rebuilds each mutant, runs the test, prints DETECTED/SURVIVED/BUILD-ERROR and restores the files even on ^C. A BUILD-ERROR is not a detection.
- **The software GPU device (QEMU) completes every EXEC at once.** It cannot show "work queued, not finished, nobody calling in" races. Put those in an `nvgpu_hw_test` section and prove them on metal; say so when a mutant survives in QEMU for that reason.
- **A flaky test that appears only in the full suite** (here: "the slot is still busy right after waitpid") can be a real pre-existing race made likelier by the change: repeat the single test in one boot first (`for i in ...; do /mnt/bin/x; done` through `qemu-debug.sh send`), then read the kernel path (`wait4` settled only on entry) before editing the test. Fix the kernel, then run the suite as many times as that failure's rate asks ("How many runs" below).
- Tests that count concurrent resources across `fork` must make every child open, report, and wait for a "go" byte: in QEMU a child finishes and releases its slot before the next one opens.

## Flakes, baselines and what a harness cannot see (2026-10-01)

- **Measure the baseline of the unchanged tree before engineering a fix** for a failure in a test. The QEMU TLB-shootdown panic looked like a regression of the new test and was 6/6 on the unchanged kernel (`scripts/tlb-stress.sh`); a long block-cache "fix" was built before that was known and then reverted. State sample sizes (n=4-5 is an order of magnitude, not a rate).
- `gui_comp_test` (3 cases, QEMU) needs most of `TEST_TIMEOUT` (default 90 s per test); it runs with 4 CPUs (`run-abi-suite.sh`).
- **A host test that compares against a reference proves only what it can observe.** `host-comp.sh` passed while the shader read 16 GiB out of range, because the host GPU returns zero for such loads. For GPU-side memory safety the only oracle is the real device and the kernel's fault report (`[nvgpu] channel .. is dead: ... address A: <where>`); say what a harness cannot see in the test's own comment.
- When a check in a job or test can fail on a healthy run (a counter printed before an asynchronous cleanup), make it informational and check the real invariant at the end (`gpu_share` 0/0/0).
- Mutation-check new pure helpers before documenting them as tested (the nvgpu fault decoders: 9 mutants, 8 killed, 1 equivalent).

## How many runs

Each run is a boot and a read of its output: run what answers the question, once.

- **One run of the narrowest thing that covers the change.** A crate: its `cargo test`. One syscall or one test program: `scripts/run-abi-suite.sh --no-build <test>` (seconds), not the whole list. Kernel code on a hardware path: `run-kernel-tests.sh`. The full suite once before calling a kernel change done, not after every edit.
- **A clean run is the answer**: do not repeat it "to be sure". Do not run TCG as well as KVM (TCG only on purpose, `qemu-debug` skill). Do not re-run to see more output: every run leaves `/tmp/abi-suite.{report,results,sysprof,backtraces}` and serial.log; read those.
- **`--no-build` when nothing changed** since the last build. (Without it the build is a no-op anyway, ~0.1-1.5 s; the boot is what costs.)
- **Repeat only to measure a rate**, and say how many runs and why:
  - A failure that might be intermittent: run *that test* in a loop in one boot (`for i in ...; do /mnt/bin/x; done` through `qemu-debug.sh send`), not N full suites; the full suite only if it does not show alone.
  - Baseline of the unchanged tree: as many runs as it takes to see the failure there (stop at the first), or as many as the fixed tree gets.
  - Claiming an intermittent fixed: enough clean runs that its old rate would almost surely have shown. A failure in fraction p of runs survives k clean runs with probability (1-p)^k: p = 3/4 needs 4 (0.4%), p = 1/10 needs about 30 of the single test in a loop (4%). Name the probe that showed the mechanism; the count only backs it up.
- **Sabotage**: one run per mutant (`gpu-mutate-qemu.py` batches them), only for a new test or a rewritten check.
- **Metal** (`metal-run` skill) is the expensive run: only what QEMU cannot show (real timers, the GPU's in-flight races, Ryzen-only hardware), one job that collects everything needed.

## Writing tests that run fast

The suite went from 163 s to ~25 s (2026-10-08) mostly by deleting waits, not by changing what is tested. The rules:

- **Wait for the condition, never for "long enough".** `nanosleep(150 ms)` "so the child is asleep" costs 150 ms on every run and still races on a slow host. `#include "testutil.h"` (in `userspace/c/`) and use:
  - `tu_wait_blocked(pid, 2000)`: until `/proc/<pid>/stat` says `S` (blocked in the kernel). Threads have pids of their own, so it works for a thread. A freshly forked child reads `R` first: always poll, never read once.
  - `tu_wait_state(pid, 'Z' | 'T' | ..., ms)`: a zombie, a stopped process.
  - `tu_wait_second_after(t)`: file timestamps are whole seconds from the same clock as `time()`; this waits for the next second (at most 1 s, 0.5 s on average) instead of `sleep(2)`.
  - `tu_now_ms()`, `tu_nap_ms()`.
  - The timeout argument turns a hang into a failed check with a message. Return values feed the check (`blocked && ...`).
- **`tu_wait_blocked` only means something if the process has one place to block** before the event. A child that sleeps, then reads, is `S` in the sleep too.
- **Replace a duration check with a stronger one where you can.** `waited >= 200 ms` meant "sigsuspend really slept"; reading the handler's flag *right after the call returns* (`handled_at_return`) proves the same with no wait. A helper that sends a signal and then waits for the handler flag before completing the wait catches a kernel that holds the signal back (`wait_intr_test`).
- **When a duration is the observable, shrink it, keep the margin relative.** `vfork_test` checks that the parent waits for the child's lifetime: 60 ms instead of 300. `cputime_test` counts 10 ms ticks: 150 ms of spin with a threshold of 12 ticks instead of 300/25. `timer_test`'s tick rate over 0.5 s (~50 ticks) against a 15% margin.
- **A child that must not exit on its own** waits on a pipe the parent writes (or closes): never `nap(400); _exit()`. Kill it instead of waiting for a long `sleep` it exec'd.
- **Stress tests are the exception**: their duration is their detection probability (`fdlock_test`'s iterations). Do not shorten one without re-running its sabotage at the new size.
- Every check message carries the values it compared (and `__LINE__` when one test has many similar checks): the FAIL line should be the whole diagnosis.
- After rewriting a test, prove it still catches what it was for: `scripts/gpu-mutate-qemu.py LIST.py` with a kernel mutant (for `wait_intr_test`: "a signal does not interrupt a blocked wait", "SA_RESTART is ignored", both DETECTED).

## Reading the suite's report

Outputs are short on purpose (read every run): a clean `run-abi-suite.sh` is three lines, a passing `run-kernel-tests.sh` prints `N cases ok` and the log's path (`QEMU_TEST_VERBOSE=1` for all of it; a failure prints the last 60 lines), a root `cargo build` about a dozen. Every exit code: `/tmp/abi-suite.results`; profiles of failed or slow (> 2 s) tests: `/tmp/abi-suite.sysprof`; backtraces of clean tests (faults on purpose): `/tmp/abi-suite.backtraces`.

`run-abi-suite.sh` ends with `scripts/abi-suite-report.py` over serial.log:

- **The slowest tests** (`SUITE_TIME`, `/proc/uptime` around each). A test new to the top of that list is a regression or a new fixed wait.
- **For each test that was not clean (counted once, whatever made it so), or was running when the run gave up: its FAIL lines, its syscall profile** from `/proc/sysprof` (per program: calls, wall time including time blocked, slowest call, `restarted` = re-executed after blocking with `rip -= 2`), and `sysprof in-flight pid N (name): read for 12.3s` for every process still in a call. A test still running 15-20 s after it started gets its live profile dumped once (`SUITE_HANG`, one watcher for the whole run), so a hang names the call it is stuck in before the host times out.
- **`user backtrace:` lines as function at file:line**, for the tests that were not clean (each one is attributed to the test running when the kernel printed it). The kernel prints a frame-pointer walk for every process it kills for a fault with no handler (`init/devices.rs`, `diag::backtrace`). C programs are built with `-fno-omit-frame-pointer -g`; the unstripped copies are in `target/userspace-syms/`. Tests that fault on purpose (`sigsegv_test`, `nx_test`, `vmshare_test`...) print some every run: that is normal.
- By hand in a booted guest: `kdebug sysprof on`, run it, `cat /proc/sysprof`; `kdebug sysprof reset` between runs.
- A test that started and finished but whose `SUITE_RESULT` line was lost (mixed with another writer on the console) is reported as `no SUITE_RESULT line` with its raw lines and counts as not clean. Seen once; read the lines when it appears.
- How the last intermittent was found (`nvgpu_sw_test`, session busy after `waitpid`): a one-line probe in the failing path printing the state that could explain it (`dead_files` pending/in flight), printing **only when that state is non-trivial** (printing every time hid the race). It named the cause in one run.

