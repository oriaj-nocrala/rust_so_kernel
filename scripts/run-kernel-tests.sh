#!/usr/bin/env bash
# QEMU integration test framework — primary entry point.
#
#   scripts/run-kernel-tests.sh
#
# Builds the kernel's test binary (`cargo build --target x86_64-unknown-none
# --tests`, run from `kernel/`), boots it in real QEMU headless with
# `-device isa-debug-exit`, and exits 0 (all `#[test_case]`s passed) or
# nonzero (a test failed, or the kernel hung/crashed before reporting).
#
# WHY NOT PLAIN `cargo test`: it doesn't work here. `cargo test
# --target x86_64-unknown-none` makes cargo build the "kernel" bin target
# TWICE within one invocation (once normally, once under `--cfg test`),
# and with this crate's `-Z build-std`, that produces two independently
# built `core` crates that collide (`error[E0152]: duplicate lang item in
# crate 'core': 'sized'`) the moment a shared dependency (spin, bitflags,
# bootloader_api, ...) is needed by both — see the comment on
# `kernel/.cargo/config.toml`'s `[target.x86_64-unknown-none] runner` key
# for the full diagnosis. `cargo build --tests` (no implicit "also build
# it normally" step) does not hit this, so that's what this script drives
# instead — same guest-side test code (`kernel/src/test_framework.rs`,
# `kernel/src/hw_tests.rs`), just invoked without `cargo test`'s own
# runner/reporting layer.
#
# This script also doubles as `kernel/.cargo/config.toml`'s
# `[target.x86_64-unknown-none] runner` — when cargo (or a future fixed
# `cargo test`) invokes it with an existing file path as `$1`, it skips
# straight to booting that ELF instead of rebuilding.
#
# The actual QEMU boot + isa-debug-exit interpretation is
# `qemu-test-runner/` (a separate, host-side, std crate — see its
# Cargo.toml for why it's not just another root-package binary). This
# script builds that on demand.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNNER_MANIFEST="$REPO_ROOT/qemu-test-runner/Cargo.toml"
RUNNER_BIN="$REPO_ROOT/qemu-test-runner/target/release/qemu-test-runner"

# Build the host-side QEMU launcher once (fast: plain std crate, one small
# dependency). `--quiet` keeps this script's own output focused on the
# kernel build + guest serial log.
cargo build --quiet --release --manifest-path "$RUNNER_MANIFEST"

if [ $# -ge 1 ] && [ -f "$1" ]; then
    # Cargo-runner invocation shape: `$1` is already a compiled test ELF —
    # skip the build below and boot it directly.
    exec "$RUNNER_BIN" "$1"
fi

echo "Building kernel test binary (cargo build --target x86_64-unknown-none --tests)..." >&2
BUILD_JSON="$(mktemp)"
trap 'rm -f "$BUILD_JSON"' EXIT

(cd "$REPO_ROOT/kernel" && cargo build --target x86_64-unknown-none --tests --message-format=json) \
    > "$BUILD_JSON"

# The test binary's filename is content-hashed (`kernel-<hash>`), not
# stable across rebuilds — pull the real path out of cargo's own build
# plan instead of guessing/globbing.
TEST_ELF="$(python3 -c "
import json, sys
for line in open('$BUILD_JSON'):
    line = line.strip()
    if not line:
        continue
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if (m.get('reason') == 'compiler-artifact'
            and m.get('target', {}).get('name') == 'kernel'
            and m.get('profile', {}).get('test')):
        print(m.get('executable') or '')
")"

if [ -z "$TEST_ELF" ] || [ ! -f "$TEST_ELF" ]; then
    echo "run-kernel-tests.sh: could not find the built test kernel ELF in cargo's output" >&2
    exit 1
fi

echo "Booting $TEST_ELF in QEMU..." >&2
exec "$RUNNER_BIN" "$TEST_ELF"
