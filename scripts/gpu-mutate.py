#!/usr/bin/env python3
"""scripts/gpu-mutate.py — hand-rolled mutation testing for one nvgpu source file.

  scripts/gpu-mutate.py nvgpu/src/chan.rs chan:: nvgpu/mutations/chan_doorbell.py

Runs `cargo test --lib <FILTER>` in `nvgpu/` once per mutation and reports the ones the tests
did NOT notice ("SURVIVED"), which are gaps: a constant or branch without an assertion.
MUTATIONS.py defines `M = [(old, new), ...]`: `old` is a substring of FILE (first occurrence,
must be before `#[cfg(test)]`), `new` its replacement. The file is restored at the end (and on
Ctrl-C). A mutation that loops forever counts as detected (240 s timeout each); one that does not
compile is reported as COMPILE ERR (write a mutation that compiles).

Write a fresh list for every change: mutate every constant (off by one, a neighbouring value),
every shift/mask, every branch condition and every field offset; equivalent mutants are
documented, not chased. Lists that were useful are kept in nvgpu/mutations/ as examples.
Do not keep helper scripts in /tmp: a metal run reboots the machine and can empty it.
"""
import os, runpy, subprocess, sys

def main():
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    path, filt, muts = sys.argv[1:]
    root = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..')
    path = os.path.abspath(path)
    M = runpy.run_path(muts)['M']
    orig = open(path).read()
    cut = orig.find('#[cfg(test)]')
    cut = len(orig) if cut < 0 else cut
    survivors = 0
    try:
        for i, (a, b) in enumerate(M):
            idx = orig.find(a)
            if idx < 0 or idx > cut:
                print(f"NOT FOUND {i}: {a[:70]!r}")
                survivors += 1
                continue
            open(path, 'w').write(orig[:idx] + b + orig[idx + len(a):])
            try:
                r = subprocess.run(['cargo', 'test', '--lib', filt], cwd=os.path.join(root, 'nvgpu'),
                                   stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=240)
                ok = r.returncode == 0
                if not ok and 'error[' in r.stdout + r.stderr and 'test result' not in r.stdout:
                    print(f"COMPILE ERR {i}: {a[:70]!r}")
            except subprocess.TimeoutExpired:
                ok = False
            if ok:
                survivors += 1
                print(f"SURVIVED {i}: {a[:70]!r} -> {b[:60]!r}")
    finally:
        open(path, 'w').write(orig)
    print(f"mutations {len(M)}, survivors {survivors}")
    sys.exit(1 if survivors else 0)

main()
