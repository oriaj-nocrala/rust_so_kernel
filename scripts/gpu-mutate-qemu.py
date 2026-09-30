#!/usr/bin/env python3
"""Sabotage of kernel code through a QEMU test: for each mutation, patch the source, rebuild, run one C test in the guest with
scripts/run-abi-suite.sh and require it to FAIL. The originals are restored at the end, even on ^C.

    scripts/gpu-mutate-qemu.py nvgpu/mutations/sharing_kernel.py

The list is a python file defining TEST (the C test's name) and MUTS = [(file, name, old, new), ...]; `old` must occur exactly once.
A mutant counts as DETECTED when the suite does not report `0 not clean` (a failing check, a panic, a stall). A build error in a
mutant is reported as BUILD-ERROR (not a detection: fix the mutation) because run-abi-suite.sh would happily run the previous binary.
"""
import runpy, subprocess, sys, os, signal

root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
os.chdir(root)
spec = runpy.run_path(sys.argv[1])
TEST, MUTS = spec["TEST"], spec["MUTS"]
originals = {}
for f, *_ in MUTS:
    if f not in originals:
        originals[f] = open(f).read()

def restore(*_):
    for f, s in originals.items():
        open(f, "w").write(s)

signal.signal(signal.SIGINT, lambda *a: (restore(), sys.exit(130)))
try:
    for f, name, old, new in MUTS:
        src = originals[f]
        if src.count(old) != 1:
            print(f"BAD-PATTERN  {name} ({src.count(old)} matches)")
            continue
        open(f, "w").write(src.replace(old, new, 1))
        b = subprocess.run(["cargo", "build"], capture_output=True, text=True, stdin=subprocess.DEVNULL)
        if b.returncode != 0:
            print(f"BUILD-ERROR  {name}")
            restore()
            continue
        os.utime("build.rs")
        r = subprocess.run(["scripts/run-abi-suite.sh", "--no-build", TEST], capture_output=True, text=True, stdin=subprocess.DEVNULL, timeout=560)
        ok = "0 not clean" in r.stdout + r.stderr
        print(("SURVIVED   " if ok else "DETECTED   ") + name, flush=True)
        restore()
finally:
    restore()
