#!/usr/bin/env bash
# Parallel QEMU boot-classification harness.
#
# Built for the SLAB_ALLOCATOR self-deadlock investigation (see CLAUDE.md /
# the debug_hang_selfdeadlock session): measuring a rare (~1-in-10 per
# operation) boot hang sequentially, one `qemu-debug.sh start` at a time, is
# slow enough to make bisection impractical. Parallelizing QEMU instances
# doesn't cost tokens — same commands, same aggregated output, just N of them
# running concurrently on the host — so it's the obvious lever.
#
# Runs N independent QEMU instances in parallel, each doing M sequential
# boots, classifying every boot by grepping its own serial.log, and
# aggregating totals at the end. Works on a stock kernel (an ordinary boot
# reaching the ash prompt counts as OK); it ALSO understands the optional
# HANG_HUNT amplifier wired into userspace/src/bin/shell.rs's `_start()` (a
# temporary, uncommitted investigation harness that repeats a suspect
# operation N times per boot, turning "1 failure in 10 boots" into "failed at
# iteration N"). This script does exactly one `cargo build` up front, then passes `--no-build` to every
# per-boot `qemu-debug.sh start` after that, both to avoid N*M redundant
# nested builds and to avoid racing multiple concurrent `cargo build`
# invocations against the same target/ dir.
#
# Isolation per instance (composes mechanisms already in qemu-debug.sh —
# see that script's own header):
#   - QEMU_DEBUG_STATE_DIR: a private serial.log/monitor.sock/pidfile dir.
#   - QEMU_DEBUG_DISK_IMG: a qcow2 overlay over the shared disk.img
#     (`qemu-img create -f qcow2 -b disk.img -F raw overlay.qcow2`) —
#     kilobytes, not a 96 MiB copy, and leaves the base image untouched.
#     One overlay per instance: they can't be shared, since the kernel
#     mounts /mnt (ext2) read-write and `reclaim_orphans` writes to it on
#     every mount.
#   - QEMU_GDB_PORT: base port + instance index, so a caller can still
#     manually attach mid-run without a collision, even though this script
#     itself never passes --gdb (a hung boot here is just killed and
#     classified HANG; if you need a backtrace for a survivor, rerun that
#     one case by hand with `qemu-debug.sh start --gdb`).
#
# Classification per boot (grepped from that boot's own serial.log):
#   OK            — the ash banner ("built-in shell (ash)") was reached, or,
#                   with the amplifier compiled in, "HANG_HUNT completed all
#                   N iterations". Matching only the latter used to report
#                   HANG for every boot of an uninstrumented kernel.
#   PANIC         — "=== KERNEL PANIC ===" seen (and not a double fault).
#   DOUBLE_FAULT  — "DOUBLE FAULT" seen (a specific panic message).
#   HANG          — neither seen before the per-boot timeout.
#
# Usage:
#   scripts/boot-matrix.sh N M [--release] [--timeout SECS] [--no-build]
#
# Output: one line per boot (instance/boot/outcome/failing-iteration-if-any),
# then an aggregate summary with a failure count and the average failing
# iteration. Exits 0 unconditionally — this is a measurement tool, not a
# pass/fail gate; read the aggregate line.

set -uo pipefail   # deliberately NOT -e: one boot's classification going
                    # sideways must not take down the rest of the matrix

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
QEMU_DEBUG="$REPO_ROOT/scripts/qemu-debug.sh"

usage() {
    echo "Usage: $0 N M [--release] [--timeout SECS] [--no-build]" >&2
    exit 1
}

[ $# -ge 2 ] || usage
N="$1"; M="$2"; shift 2
PROFILE_FLAG=()
TIMEOUT=200
DO_BUILD=1
while [ $# -gt 0 ]; do
    case "$1" in
        --release) PROFILE_FLAG=(--release) ;;
        --timeout) TIMEOUT="$2"; shift ;;
        --no-build) DO_BUILD=0 ;;
        *) echo "unknown arg: $1" >&2; usage ;;
    esac
    shift
done

case "$N$M" in *[!0-9]*) usage ;; esac

WORK="/tmp/boot-matrix-$$"
mkdir -p "$WORK"
echo "work dir: $WORK  (N=$N instances, M=$M boots each, timeout=${TIMEOUT}s)" >&2

if [ "$DO_BUILD" = 1 ]; then
    echo "Building once (profile: ${PROFILE_FLAG[*]:-debug})..." >&2
    if ! (cd "$REPO_ROOT" && cargo build "${PROFILE_FLAG[@]}"); then
        echo "build failed" >&2
        exit 1
    fi
fi

# Same output-dir lookup qemu-debug.sh's find_output_file()/cmd_start use.
OUT_FILE="$(find "$REPO_ROOT/target" -maxdepth 4 -path "*/build/so2-*/output" -printf '%T@ %p\n' 2>/dev/null \
    | sort -rn | head -1 | cut -d' ' -f2-)"
if [ -z "$OUT_FILE" ]; then
    echo "no build output found under target/ — run without --no-build first" >&2
    exit 1
fi
BASE_DISK="$(grep -oP 'EXT2_DISK_PATH=\K.*' "$OUT_FILE")"
if [ -z "$BASE_DISK" ] || [ ! -f "$BASE_DISK" ]; then
    echo "could not locate base disk.img via $OUT_FILE" >&2
    exit 1
fi
echo "base disk image: $BASE_DISK" >&2

BASE_GDB_PORT=13340

run_instance() {
    local idx="$1"
    local sd="$WORK/inst-$idx"
    local overlay="$WORK/overlay-$idx.qcow2"
    local results="$WORK/results-$idx.txt"
    mkdir -p "$sd"
    : > "$results"

    if ! qemu-img create -f qcow2 -b "$BASE_DISK" -F raw "$overlay" >"$sd/qemu-img.log" 2>&1; then
        echo "inst=$idx: qemu-img overlay creation failed, see $sd/qemu-img.log" >&2
        for boot in $(seq 1 "$M"); do
            echo "inst=$idx boot=$boot outcome=OVERLAY_FAILED iter=" >> "$results"
        done
        return
    fi

    for boot in $(seq 1 "$M"); do
        export QEMU_DEBUG_STATE_DIR="$sd"
        export QEMU_DEBUG_DISK_IMG="$overlay"
        export QEMU_GDB_PORT=$((BASE_GDB_PORT + idx))

        "$QEMU_DEBUG" stop >/dev/null 2>&1 || true

        if ! "$QEMU_DEBUG" start --no-build >/dev/null 2>&1; then
            echo "inst=$idx boot=$boot outcome=START_FAILED iter=" >> "$results"
            continue
        fi

        # Success is EITHER the amplifier finishing its iterations (when
        # `HANG_HUNT` instrumentation is compiled in) OR an ordinary boot
        # reaching the interactive shell. Matching only the former made every
        # run of a stock, uninstrumented kernel report HANG — 20/20 false
        # positives, on boots whose serial logs plainly ended at the ash
        # banner. A measurement tool that reports 100% failure on a healthy
        # kernel is worse than no tool, so both are accepted here.
        local outcome iter
        if "$QEMU_DEBUG" wait-for "HANG_HUNT completed|built-in shell \(ash\)|KERNEL PANIC" "$TIMEOUT" >/dev/null 2>&1; then
            if grep -q "DOUBLE FAULT" "$sd/serial.log" 2>/dev/null; then
                outcome=DOUBLE_FAULT
            elif grep -q "KERNEL PANIC" "$sd/serial.log" 2>/dev/null; then
                outcome=PANIC
            else
                outcome=OK
            fi
        else
            outcome=HANG
        fi

        iter="$(grep -o 'hunt: iter [0-9]* begin' "$sd/serial.log" 2>/dev/null | tail -1 | grep -o '[0-9]*' || true)"
        # CPUs up, from the kernel's own `smp:` line (stage 4 of the SMP
        # plan): reaching the shell says nothing about whether every AP came
        # up, so each boot records it and the aggregate shows the spread.
        cpus="$(grep -a -oP 'smp: madt \K[0-9]+ cpus, [0-9]+(?= online)' "$sd/serial.log" 2>/dev/null | head -1 | sed 's/ cpus, /:/' || true)"
        echo "inst=$idx boot=$boot outcome=$outcome iter=$iter cpus_online=${cpus#*:}/${cpus%%:*}" >> "$results"

        # Preserve the serial log of every non-OK boot (rare enough to be
        # cheap) before the next boot in this instance truncates it —
        # useful for spot-checking a HANG/PANIC's exact tail without
        # needing to rerun it.
        if [ "$outcome" != "OK" ]; then
            cp "$sd/serial.log" "$WORK/serial-inst${idx}-boot${boot}-${outcome}.log" 2>/dev/null || true
        fi

        "$QEMU_DEBUG" stop >/dev/null 2>&1 || true
    done
}

pids=()
for idx in $(seq 1 "$N"); do
    run_instance "$idx" &
    pids+=($!)
done
for pid in "${pids[@]}"; do
    wait "$pid"
done

echo
echo "=== per-boot results ==="
cat "$WORK"/results-*.txt 2>/dev/null | sort -t= -k2 -n

echo
echo "=== aggregate ==="
total=0; ok=0; hang=0; panic=0; dfault=0; other=0
fail_iters=()
while IFS= read -r line; do
    [ -z "$line" ] && continue
    total=$((total+1))
    outcome="$(echo "$line" | grep -oP 'outcome=\K[A-Z_]+')"
    iter="$(echo "$line" | grep -oP 'iter=\K[0-9]*' || true)"
    case "$outcome" in
        OK) ok=$((ok+1)) ;;
        HANG) hang=$((hang+1)); [ -n "$iter" ] && fail_iters+=("$iter") ;;
        PANIC) panic=$((panic+1)); [ -n "$iter" ] && fail_iters+=("$iter") ;;
        DOUBLE_FAULT) dfault=$((dfault+1)); [ -n "$iter" ] && fail_iters+=("$iter") ;;
        *) other=$((other+1)) ;;
    esac
done < <(cat "$WORK"/results-*.txt 2>/dev/null)

echo "total=$total ok=$ok hang=$hang panic=$panic double_fault=$dfault other=$other"
echo "cpus online/madt: $(cat "$WORK"/results-*.txt 2>/dev/null | grep -oP 'cpus_online=\K\S+' | sort | uniq -c | awk '{printf "%s x%s  ", $2, $1}')"
failed=$((hang+panic+dfault+other))
if [ "$total" -gt 0 ]; then
    rate=$(awk "BEGIN{printf \"%.1f\", 100.0*$failed/$total}")
    echo "failure_rate=${failed}/${total} (${rate}%)"
fi
if [ "${#fail_iters[@]}" -gt 0 ]; then
    sum=0
    for v in "${fail_iters[@]}"; do sum=$((sum+v)); done
    avg=$(awk "BEGIN{printf \"%.1f\", $sum/${#fail_iters[@]}}")
    echo "failing iterations: ${fail_iters[*]}  (avg $avg, n=${#fail_iters[@]})"
fi

echo
echo "non-OK serial logs preserved under: $WORK (serial-inst*-boot*-*.log)"
echo "cleaning up state dirs + overlays (keeping results/serial logs)..."
for idx in $(seq 1 "$N"); do
    rm -rf "$WORK/inst-$idx" "$WORK/overlay-$idx.qcow2"
done
echo "done. work dir: $WORK"
