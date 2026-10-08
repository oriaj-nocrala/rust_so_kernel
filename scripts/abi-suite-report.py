#!/usr/bin/env python3
"""What scripts/run-abi-suite.sh prints after a run, from the guest's serial.log:

  - the slowest tests (SUITE_TIME: /proc/uptime before and after each one);
  - for every test that was not clean, or was running when the run was aborted: its syscall profile (SUITE_PROF /
    SUITE_HANG lines from /proc/sysprof: calls, wall time and slowest call per syscall, and the call each live process
    is in right now) and its FAIL lines;
  - the `user backtrace:` lines the kernel printed (a process killed for a fault) during those tests, as function at
    file:line (addr2line on the unstripped copies kernel/build.rs leaves in target/userspace-syms/).

Short on purpose (it is read by people and agents every run): a clean run is two lines. Writes every test's full profile
to /tmp/abi-suite.sysprof and the raw backtraces of clean tests to /tmp/abi-suite.backtraces. The last line is
`SUITE_BAD <n>`, the number of tests that were not clean, for the runner.
  scripts/abi-suite-report.py SERIAL_LOG [RUNNING_TEST]
"""
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SYMS = os.path.join(ROOT, "target", "userspace-syms")
PLUMBING = {"shell", "sh", "sed", "cut", "kdebug", "sleep", "exe"}  # the suite's own processes (PID 1 is "shell")


def main():
    log = open(sys.argv[1], errors="replace").read().splitlines()
    running = sys.argv[2] if len(sys.argv) > 2 else ""

    times, results, prof, fails, started, traces = {}, {}, {}, {}, [], {}
    current = None
    for raw in log:
        if "user backtrace:" in raw:
            # A kernel line, interleaved with the console: it belongs to the test running at that point.
            traces.setdefault(current, []).append(raw[raw.index("user backtrace:"):])
            continue
        if not raw.startswith("[fb] "):
            continue
        l = raw[5:]
        m = re.match(r"SUITE_START (\w+)", l)
        if m:
            current = m.group(1)
            started.append(current)
            continue
        m = re.match(r"SUITE_RESULT (\w+)=(\d+)", l)
        if m:
            results[m.group(1)] = int(m.group(2))
            continue
        m = re.match(r"SUITE_TIME (\w+) ([\d.]+) ([\d.]+)", l)
        if m:
            times[m.group(1)] = (float(m.group(3)) - float(m.group(2))) * 1000
            continue
        m = re.match(r"SUITE_(PROF|HANG) (\w+) (.*)", l)
        if m:
            prof.setdefault(m.group(2), []).append(("HANG " if m.group(1) == "HANG" else "") + m.group(3))
            continue
        if current and "FAIL" in l:
            fails.setdefault(current, []).append(l)

    with open("/tmp/abi-suite.sysprof", "w") as f:
        for t, lines in prof.items():
            f.write(f"== {t}\n" + "".join(x + "\n" for x in lines))

    if times:
        slow = sorted(times.items(), key=lambda kv: -kv[1])[:3]
        print(f"time: {sum(times.values()) / 1000:.1f} s in {len(times)} tests; slowest: "
              + ", ".join(f"{t} {ms / 1000:.1f}s" for t, ms in slow))

    # Not clean, each test once: a nonzero exit, a FAIL line, the test the run gave up in, or a result line that never
    # matched (the console mixed it with another writer: its raw lines say what happened).
    bad = [t for t in started if results.get(t, 0) != 0 or t in fails or t == running or t not in results]
    for t in dict.fromkeys(bad):
        if t in results:
            what = f"exit {results[t]}"
        elif t == running:
            what = "still running when the run gave up"
        else:
            what = "no SUITE_RESULT line (garbled on the console?)"
        print(f"--- {t}: {what}")
        if t not in results and t != running:
            raw = [l for l in log if l.startswith("[fb] ") and t in l and "SUITE_PROF" not in l and "SUITE_HANG" not in l]
            for l in raw[:8]:
                print("  " + repr(l))
        for l in list(dict.fromkeys(fails.get(t, [])))[:10]:
            print("  " + l)
        print_profile(t, prof.get(t, []))
        for tr in traces.get(t, []):
            print_backtrace(tr)

    # Backtraces in clean tests are faults the tests cause on purpose (sigsegv_test, nx_test...): kept out of the way.
    quiet = [(t, tr) for t, trs in traces.items() if t not in bad for tr in trs]
    if quiet:
        with open("/tmp/abi-suite.backtraces", "w") as f:
            for t, tr in quiet:
                f.write(f"== {t}\n{tr}\n")
        print(f"({len(quiet)} user backtraces from clean tests, expected: /tmp/abi-suite.backtraces)")
    print(f"SUITE_BAD {len(dict.fromkeys(bad))}")


def print_profile(test, lines):
    """The test's own programs' blocks (not the suite's plumbing), and every in-flight line."""
    if not lines:
        print("  (no syscall profile: it exited 0 and ran under 2 s, or did not finish and ran under 15 s)")
        return
    show = False
    for l in lines:
        body = l[5:] if l.startswith("HANG ") else l
        m = re.match(r"sysprof (\S+) procs=", body)
        if m:
            show = m.group(1) not in PLUMBING
        if body.startswith("sysprof in-flight"):
            show_line = not re.search(r"\((%s)\)" % "|".join(PLUMBING), body)
        else:
            show_line = show
        if show_line:
            print("  " + l)


def syms_for(name):
    """The unstripped binary for a process name (comm is cut at 15 characters)."""
    if not os.path.isdir(SYMS):
        return None
    for f in sorted(os.listdir(SYMS)):
        if f == name or (len(name) >= 15 and f.startswith(name)):
            return os.path.join(SYMS, f)
    return None


def print_backtrace(line):
    print("--- " + line)
    m = re.match(r"user backtrace: pid \d+ \(([^)]*)\)", line)
    pcs = re.findall(r"#(\d+) (0x[0-9a-f]+)", line)
    elf = syms_for(m.group(1)) if m else None
    if not elf or not pcs:
        print("  (no symbols: target/userspace-syms/ has no binary for this name)")
        return
    # A return address points after the call: look up the byte before it to get the call's line.
    addrs = [pc if i == "0" else hex(int(pc, 16) - 1) for i, pc in pcs]
    try:
        out = subprocess.run(["addr2line", "-f", "-C", "-e", elf] + addrs, capture_output=True, text=True).stdout.split("\n")
    except FileNotFoundError:
        print("  (addr2line not installed)")
        return
    for k, (i, pc) in enumerate(pcs):
        fn, loc = out[2 * k] if 2 * k < len(out) else "?", out[2 * k + 1] if 2 * k + 1 < len(out) else "?"
        print(f"  #{i} {pc} {fn} at {loc.replace(ROOT + '/', '')}")


if __name__ == "__main__":
    main()
