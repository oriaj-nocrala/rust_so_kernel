#!/usr/bin/env python3
"""scripts/gpu-trace.py — read the phase 0 oracle traces (docs/gpu/gpu-plan.md).

A trace dir is what scripts/gpu-oracle.sh leaves: mmiotrace.txt + dmesg.txt.

  gpu-trace.py align   DIR                  clock offset mmiotrace - dmesg, from two anchors
  gpu-trace.py segment DIR [--min-acc N]    one row per run of nouveau messages of one
                                            subsystem: dmesg time span, R/W counts, hottest
                                            BAR0 64 KiB blocks
  gpu-trace.py extract DIR T0 T1 [OUT]      trace lines between dmesg times T0..T1 (seconds)
  gpu-trace.py regs    DIR T0 T1 [--top N]  per-register R/W counts and first/last value
                                            in a dmesg time window

Clock alignment. mmiotrace and printk stamp with different clocks. The
anchor is the VBIOS PROM read (BAR0 0x300000..0x3fffff): nouveau prints
"bios: trying PROM..." right before its first PROM read and "bios: scored"
right after its last one (nvkm/subdev/bios/shadow.c). Both give the offset;
`align` fails if they disagree by more than 5 ms.

mmiotrace line format (kernel Documentation/trace/mmiotrace.rst):
  R|W width time map_id phys_addr value pc pid
"""
import re
import sys
from collections import Counter, defaultdict

PROM_LO, PROM_HI = 0x300000, 0x400000
NV_LINE = re.compile(r"^\[\s*([0-9.]+)\] nouveau [0-9a-f:.]+: (.*)$")


def load_dmesg(d):
    out = []
    for line in open(f"{d}/dmesg.txt", errors="replace"):
        m = NV_LINE.match(line.rstrip("\n"))
        if m:
            out.append((float(m.group(1)), m.group(2)))
    return out


def iter_trace(d):
    """(kind, time, phys, value) for every R/W line."""
    with open(f"{d}/mmiotrace.txt") as f:
        for line in f:
            if line[0] not in "RW" or line[1] != " ":
                continue
            p = line.split()
            yield p[0], float(p[2]), int(p[4], 16), int(p[5], 16)


def bar0_base(d):
    for line in open(f"{d}/mmiotrace.txt"):
        if line.startswith("MAP ") and line.split()[5] == "0x1000000":
            return int(line.split()[3], 16)
    sys.exit("no 16 MiB BAR0 MAP line in the trace")


def align(d, quiet=False):
    dm = load_dmesg(d)
    try:
        t_start = next(t for t, m in dm if m.startswith("bios: trying PROM"))
        t_end = next(t for t, m in dm if m.startswith("bios: scored"))
    except StopIteration:
        sys.exit("dmesg lacks the PROM anchors (bios: trying PROM / bios: scored)")
    base = bar0_base(d)
    first = last = None
    for k, t, a, _ in iter_trace(d):
        if k == "R" and base + PROM_LO <= a < base + PROM_HI:
            if first is None:
                first = t
            last = t
        elif first is not None and t - last > 0.5:
            break  # past the shadowing; later PROM reads are not the anchor
    off_a, off_b = first - t_start, last - t_end
    if abs(off_a - off_b) > 0.005:
        sys.exit(f"anchors disagree: {off_a:.4f} vs {off_b:.4f}")
    off = (off_a + off_b) / 2
    if not quiet:
        print(f"offset (mmiotrace - dmesg) = {off:.4f} s  [start {off_a:.4f}, end {off_b:.4f}]")
        print(f"BAR0 at {base:#x}")
    return off, base


def subsystem(msg):
    m = re.match(r"^([a-z0-9_]+(?:\([a-z0-9_]+\))?(?::[a-z0-9_-]+)?):", msg)
    if m:
        return m.group(1)
    return "[drm]" if msg.startswith("[drm]") else "?"


def classify(msg):
    """(key, starts_new_segment). Runs of one key merge, except that each
    display supervisor event (a modeset step, followed by its core-channel
    method dump) is its own segment, and the disp DCB/ctor lines are split
    from the rest of disp."""
    k = subsystem(msg)
    if k == "disp":
        body = msg[len("disp: "):]
        if body.startswith("supervisor"):
            return "disp:" + " ".join(body.split()[:2]).rstrip(":"), True
        if re.match(r"(outp|conn) ", body):
            return "disp:dcb", False
        if re.match(r"(Window|Head|SOR)\(s\)|(head|SOR)-\d+: ctor", body):
            return "disp:ctor", False
        return "disp", False
    return k, False


def segment(d, min_acc=1):
    off, base = align(d, quiet=True)
    dm = load_dmesg(d)
    runs = []  # [key, t0, first_msg, n_msgs]
    last = None
    for t, m in dm:
        k, new = classify(m)
        # Lines following a supervisor header (head-N:, Core:, method dumps)
        # belong to it.
        if last and last.startswith("disp:supervisor") and k == "disp":
            k = last
        last = k
        if runs and runs[-1][0] == k and not new:
            runs[-1][3] += 1
        else:
            runs.append([k, t, m, 1])
    bounds = [r[1] for r in runs] + [float("inf")]
    stats = [dict(R=0, W=0, blocks=Counter()) for _ in runs]
    i = 0
    for k, t, a, _ in iter_trace(d):
        t -= off
        if t < bounds[0]:
            i = -1
        else:
            while t >= bounds[i + 1]:
                i += 1
        if i < 0:
            continue
        s = stats[i]
        s[k] += 1
        if base <= a < base + 0x1000000:
            s["blocks"][(a - base) >> 16] += 1
    print(f"# {d}  (offset {off:.4f} s; times are dmesg time)")
    print(f"{'t0':>9} {'dt':>8} {'msgs':>5} {'R':>8} {'W':>8}  {'subsys':<14} hottest BAR0 blocks / first message")
    for (k, t0, msg, n), t1, s in zip(runs, bounds[1:], stats):
        if s["R"] + s["W"] < min_acc:
            continue
        dt = (t1 - t0) if t1 != float("inf") else 0
        hot = " ".join(f"{b << 16:06x}:{c}" for b, c in s["blocks"].most_common(4))
        print(f"{t0:9.4f} {dt:8.4f} {n:5d} {s['R']:8d} {s['W']:8d}  {k:<14} {hot}")
        print(f"{'':>44}  | {msg[:110]}")


def extract(d, t0, t1, out=None):
    off, _ = align(d, quiet=True)
    lo, hi = t0 + off, t1 + off
    f = open(out, "w") if out else sys.stdout
    n = 0
    with open(f"{d}/mmiotrace.txt") as src:
        for line in src:
            if line[0] in "RW" and line[1] == " ":
                t = float(line.split()[2])
                if t > hi:
                    break
                if t >= lo:
                    f.write(line)
                    n += 1
    if out:
        print(f"{n} lines -> {out}")


def regs(d, t0, t1, top=40):
    off, base = align(d, quiet=True)
    lo, hi = t0 + off, t1 + off
    cnt = defaultdict(lambda: [0, 0, None, None])
    for k, t, a, v in iter_trace(d):
        if t > hi:
            break
        if t < lo:
            continue
        c = cnt[a]
        c[0 if k == "R" else 1] += 1
        if c[2] is None:
            c[2] = (k, v)
        c[3] = (k, v)
    print(f"{'reg':>10} {'R':>7} {'W':>7}  first            last")
    rows = sorted(cnt.items(), key=lambda kv: -(kv[1][0] + kv[1][1]))[:top]
    for a, (r, w, first, last) in rows:
        name = f"{a - base:#08x}" if base <= a < base + 0x1000000 else f"{a:#x}"
        print(f"{name:>10} {r:7d} {w:7d}  {first[0]} {first[1]:#010x}  {last[0]} {last[1]:#010x}")


def main(argv):
    if len(argv) < 3:
        sys.exit(__doc__)
    cmd, d = argv[1], argv[2].rstrip("/")
    if cmd == "align":
        align(d)
    elif cmd == "segment":
        n = int(argv[argv.index("--min-acc") + 1]) if "--min-acc" in argv else 1
        segment(d, n)
    elif cmd == "extract":
        extract(d, float(argv[3]), float(argv[4]), argv[5] if len(argv) > 5 else None)
    elif cmd == "regs":
        n = int(argv[argv.index("--top") + 1]) if "--top" in argv else 40
        regs(d, float(argv[3]), float(argv[4]), n)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv)
