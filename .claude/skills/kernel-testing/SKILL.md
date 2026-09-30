---
name: kernel-testing
description: Playbook for verifying a change to rust_so_kernel: which test suite covers which crate, why a root cargo build proves nothing for the extracted crates, running and adding QEMU integration tests (#[test_case] in kernel/src/hw_tests.rs via scripts/run-kernel-tests.sh), userspace C test programs, the A/B/C concurrency-test taxonomy and proving a test by sabotage. Use before declaring any change done, when adding tests, or when a subagent reports test results. Keywords: cargo test, run-kernel-tests.sh, hw_tests, test_case, host tests, sabotage, verify.
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
| the C userspace tests (all of them, one boot) | `scripts/run-abi-suite.sh [--no-build] [--keep-going] [test ...]`: ~2 min for the standard 40; **fails fast** (first `FAIL`, kernel panic, a test over `TEST_TIMEOUT`=90 s, no output for `STALL`=60 s) and names the test; `KEEP_ALIVE=1` leaves a hung guest running; the list is `disk-image-root/abi-suite.sh` |
| real Rust std / tokio programs | `scripts/run-std-probe.sh` (`probes/std/g1_std.rs`: hard links, statx, `Command` with uid/gid), `scripts/run-tokio-probe.sh` (`probes/tokio`); both need `rustc` run from inside the repo (pinned nightly with the musl std) |
| the compositor or GUI programs | `scripts/gui-e2e.sh [term\|wm\|text]` |
| anything the Ryzen does differently | the `metal-run` skill |

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

- **Plain `cargo test --target x86_64-unknown-none` does not work**: it builds the bin target twice, and with `-Z build-std` that gives two `core` crates (`E0152 duplicate lang item`). This script is the replacement, and it is also the target's `runner`.
- Guest side: `kernel/src/test_framework.rs`, `kernel/src/hw_tests.rs` (every case, with a doc comment), `kernel/src/init/test_support.rs::boot_for_tests` (the part of the real boot the tests need, including APIC and APs). Host side: `qemu-test-runner/`.

### Adding a case

1. Add `#[test_case] fn name()` to `kernel/src/hw_tests.rs`. Its doc comment says what it covers and why a host test can't cover it.
2. If it needs more of the boot running, extend `boot_for_tests`.
3. Prefer RAM stand-ins for devices (`hal::block::MemDisk`, a RAM-backed `Framebuffer` with `stride > width` and a pre-filled buffer), so the test can also check that nothing outside its target was touched.
4. Sabotage it once and watch it fail.

## Userspace test programs

- `userspace/c/<thing>_test.c`, in `DISK_C_PROGRAMS` (see the `userspace-programs` skill). Each one prints its cases and a pass/fail summary. Run it in a booted kernel and grep serial.log.
- Existing ones cover sockets, ptys, shm, pipes, signals and waits, sessions, CPU time, RSS, timestamps, FPU and input polling. Look for the matching test before writing a new one.
