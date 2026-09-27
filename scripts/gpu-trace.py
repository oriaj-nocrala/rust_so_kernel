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
  gpu-trace.py aux     DIR CH [SEL OUT]     DP AUX transactions on channel CH (GM200+
                                            registers 0xd930 + CH*0x50 ...): one row each;
                                            with SEL OUT (SEL like "2,4-5,15-38"), write those
                                            transactions as a replay fixture ("R|W offset value",
                                            BAR0-relative)
  gpu-trace.py i2c     DIR DRIVE            decode bit-banged I2C on 0xd014 + DRIVE*0x20 from the
                                            line levels the driver sensed: one row per message
  gpu-trace.py core    DIR [N OUT]          the core-channel state nouveau dumps to dmesg at each
                                            supervisor 1 ("prev -> next" = ARMED -> ASSEMBLY,
                                            nvkm/engine/disp/nv50.c nv50_disp_mthd_list), named
                                            from clc67d.h; with N OUT, write dump N as a fixture
  gpu-trace.py disp    DIR T0 T1            display writes between dmesg times T0..T1, one row
                                            each, labelled with the nouveau code that owns the
                                            register (see DISP_CLASSES); AUX/I2C/timer excluded
  gpu-trace.py mem     DIR LO HI T0 T1      non-zero writes into physical [LO, HI) (a MAP line's
                                            range, e.g. BAR3 instance memory), offsets from LO

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


AUX_REGS = {0x00: "wdata", 0x04: "wdata", 0x08: "wdata", 0x0c: "wdata",
            0x10: "rdata", 0x14: "rdata", 0x18: "rdata", 0x1c: "rdata",
            0x20: "addr", 0x24: "ctrl", 0x28: "stat", 0x38: "autodpcd"}


def aux_ops(d, ch):
    """(time, kind, offset, value) for channel CH's registers, BAR0-relative.
    Register block: nvkm/subdev/i2c/auxgm200.c (0xd930..0xd968 + ch*0x50)."""
    _, base = align(d, quiet=True)
    lo = 0xd930 + ch * 0x50
    for k, t, a, v in iter_trace(d):
        o = a - base
        if lo <= o < lo + 0x3c and (o - lo) in AUX_REGS:
            yield t, k, o, v


def aux_split(ops, ch):
    """Transactions, cut after the fini write (auxgm200.c: `nvkm_mask(0xd954,
    0x00710000, 0)`, which follows the autodpcd write and a ctrl read)."""
    ctrl, auto = 0xd954 + ch * 0x50, 0xd968 + ch * 0x50
    cur = []
    for op in ops:
        cur.append(op)
        if (op[1] == "W" and op[2] == ctrl and len(cur) >= 3
                and cur[-2][1:3] == ("R", ctrl) and cur[-3][1:3] == ("W", auto)):
            yield cur
            cur = []


def parse_sel(sel):
    out = []
    for part in sel.split(","):
        a, _, b = part.partition("-")
        out.extend(range(int(a), int(b or a) + 1))
    return out


def aux(d, ch, sel=None, out=None):
    off, _ = align(d, quiet=True)
    txs = list(aux_split(aux_ops(d, ch), ch))
    if out is not None:
        with open(out, "w") as f:
            f.write(f"# {d.rsplit('/', 1)[-1]}: AUX ch {ch}, transactions {sel} "
                    f"(scripts/gpu-trace.py aux {d.rsplit('/', 1)[-1]} {ch} {sel} ...)\n")
            for tx in (txs[i] for i in parse_sel(sel)):
                for t, k, o, v in tx:
                    f.write(f"{k} {o:#06x} {v:#010x}\n")
        print(f"{len(parse_sel(sel))} transactions -> {out}")
        return
    for i, tx in enumerate(txs):
        addr = [v for _, k, o, v in tx if k == "W" and o == 0xd950 + ch * 0x50]
        go = [v for _, k, o, v in tx if k == "W" and o == 0xd954 + ch * 0x50 and v & 0x10000]
        stat = [v for _, k, o, v in tx if k == "R" and o == 0xd958 + ch * 0x50]
        rd = [v for _, k, o, v in tx if k == "R" and 0xd940 + ch * 0x50 <= o < 0xd950 + ch * 0x50]
        wr = [v for _, k, o, v in tx if k == "W" and 0xd930 + ch * 0x50 <= o < 0xd940 + ch * 0x50]
        c = go[0] if go else 0
        size = 0 if c & 0x100 else (c & 0xff) + 1
        data = b"".join(x.to_bytes(4, "little") for x in (rd or wr))[:size].hex()
        print(f"{i:4d} {tx[0][0] - off:10.6f} type {(c >> 12) & 0xf:x} addr {addr[0] if addr else 0:#07x} "
              f"size {size:2d} stat {stat[-1] if stat else 0:#010x} {'rd' if rd else 'wr' if wr else '  '} {data}")


def i2c(d, drive):
    """Sniff the bus from what the driver sensed (busgf119.c: SCL drive bit 0,
    SDA drive bit 1, SCL sense bit 4, SDA sense bit 5). Lines are open drain:
    a write of 0 pulls the line low; a write of 1 only releases it, and its
    level is known at the next read (the other side may hold it low)."""
    off, base = align(d, quiet=True)
    reg = base + 0xd014 + drive * 0x20
    scl = sda = 1
    msgs, cur, bits, t0, pending = [], None, [], 0.0, False
    for k, t, a, v in iter_trace(d):
        if a != reg:
            continue
        if k == "W":
            nscl, nsda = scl & (v & 1), sda & ((v >> 1) & 1)
        else:
            nscl, nsda = (v >> 4) & 1, (v >> 5) & 1
        if scl and nscl and sda and not nsda:          # START / repeated START
            if cur is not None and cur[1]:
                msgs.append(cur)
            cur, bits, t0, pending = [t, []], [], t, False
        elif scl and nscl and not sda and nsda:        # STOP
            if cur is not None:
                msgs.append(cur)
            cur, bits, pending = None, [], False
        elif not scl and nscl:                         # SCL released: sample at the next read
            pending = True
        if pending and k == "R" and nscl and cur is not None:
            pending = False
            bits.append(nsda)
            if len(bits) == 9:
                cur[1].append((sum(b << (7 - i) for i, b in enumerate(bits[:8])), bits[8]))
                bits = []
        scl, sda = nscl, nsda
    for t, by in msgs:
        if not by:
            continue
        a = by[0][0]
        body = bytes(b for b, _ in by[1:])
        acks = "".join("n" if nak else "a" for _, nak in by)
        print(f"{t - off:10.6f} addr {a >> 1:#04x} {'rd' if a & 1 else 'wr'} {len(body):3d} [{acks[:4]}...] {body.hex()}")


CLC67D = "src/common/sdk/nvidia/inc/class/clc67d.h"
GPU_REF = __import__("os").path.expanduser("~/src/gpu-ref")
DEF = re.compile(r"#define (NVC67D_\w+?)(\((a)\)|\((a),\s*(b)\))?\s+\((0x[0-9A-Fa-f]+)"
                 r"(?:\s*\+\s*\(a\)\*(0x[0-9A-Fa-f]+))?(?:\s*\+\s*\(b\)\*(0x[0-9A-Fa-f]+))?\)")
FIELD = re.compile(r"#define NVC67D_\w+\s+\d+:\d+")


def core_names():
    """{method offset: (name, clc67d.h line)} for every GA102 core-channel
    method: a define whose next line is a bit field."""
    path = f"{GPU_REF}/open-gpu-kernel-modules/{CLC67D}"
    lines = open(path).read().splitlines()
    out = {}
    # Two-index methods (a, b) have no bound for b in the header; they only
    # fill offsets that no one-index method claims.
    for two in (False, True):
        for i, line in enumerate(lines[:-1]):
            m = DEF.match(line)
            if not m or not FIELD.match(lines[i + 1]) or bool(m.group(5)) != two:
                continue
            name, base = m.group(1)[len("NVC67D_"):], int(m.group(6), 16)
            sa, sb = (int(x, 16) if x else 0 for x in m.group(7, 8))
            for a in range(8 if m.group(3) or m.group(4) else 1):
                for b in range(4 if two else 1):
                    idx = f"({a},{b})" if two else f"({a})" if m.group(3) else ""
                    out.setdefault(base + a * sa + b * sb, (name + idx, i + 1))
    return out


DUMP = re.compile(r"^disp: \t([0-9a-f]{4}): ([0-9a-f]{8})(?: -> ([0-9a-f]{8}))?")


def core_dumps(d):
    """[(dmesg time, [(method, armed, assembly)])], one per supervisor 1."""
    dumps, cur = [], None
    for t, m in load_dmesg(d):
        if m.startswith("disp: supervisor 1:"):
            cur = (t, [])
            dumps.append(cur)
        elif m.startswith("disp: supervisor"):
            cur = None
        elif cur is not None:
            x = DUMP.match(m)
            if x:
                armed = int(x.group(2), 16)
                cur[1].append((int(x.group(1), 16), armed,
                               int(x.group(3), 16) if x.group(3) else armed))
    return dumps


def core(d, n=None, out=None):
    names = core_names()
    dumps = core_dumps(d)
    sel = [n] if n is not None else range(len(dumps))
    f = open(out, "w") if out else sys.stdout
    for i in sel:
        t, rows = dumps[i]
        if out:
            f.write(f"# {d.rsplit('/', 1)[-1]}: core channel (GA102_DISP_CORE_CHANNEL_DMA, c67d) at "
                    f"supervisor 1, dmesg {t:.6f} (scripts/gpu-trace.py core {d.rsplit('/', 1)[-1]} {i} ...)\n"
                    f"# method armed assembly name clc67d.h:line; armed = state before this update\n")
        else:
            print(f"== dump {i}: supervisor 1 at {t:.6f} ({len(rows)} methods)")
        for mthd, armed, assy in rows:
            name, line = names.get(mthd, (None, 0))
            where = f"{name} clc67d.h:{line}" if name else "(not a c67d method)"
            mark = "*" if armed != assy else " "
            f.write(f"{mthd:04x} {armed:08x} {assy:08x} {mark} {where}\n")
    if out:
        print(f"dump {n} -> {out}")


# Display register ranges, first match wins: (lo, hi, label, owner in
# nouveau v7.2.2 nvkm/engine/disp/ unless said otherwise).
DISP_CLASSES = [
    (0x00e86c, 0x00e870, "unknown", "written once (2) at SOR acquire; no match in nouveau v7.2.2"),
    (0x00e9c0, 0x00e9d0, "vpll", "subdev/devinit/ga100.c:55 (0x00e9c0 + head*4)"),
    (0x00ec00, 0x00ed00, "sor-clk", "ga102.c:96-97 ga102_sor_clock (0xec04/0xec08 + sor*0x10)"),
    (0x00ef00, 0x00f000, "vpll", "subdev/devinit/ga100.c:52-54 (0x00ef00 + head*0x40)"),
    (0x10ec00, 0x10f000, "hda-eld", "gf119.c:53-56 (0x10ec00 + sor*0x30)"),
    (0x6013d4, 0x6013d6, "vga-crtc", "legacy VGA CR index/data (priv I/O window)"),
    (0x610010, 0x610018, "inst", "tu102_disp_init: instance memory target|addr>>16"),
    (0x610078, 0x61007c, "init", "tu102_disp_init"),
    (0x6104e0, 0x610600, "chan-ctl", "gv100.c dmac/core init+fini (0x6104e0 + ctrl*4)"),
    (0x610600, 0x610620, "curs-ctl", "gv100.c:587 gv100_disp_curs_init (ctrl 73+head)"),
    (0x6107a8, 0x6107bc, "super-ack", "gv100.c:835 gv100_disp_super (0x6107a8, 0x6107ac+head*4)"),
    (0x610b20, 0x610e00, "chan-push", "gv100.c:760/373 push address (0x610b20 + ctrl*0x10)"),
    (0x611000, 0x611e00, "intr", "gv100.c gv100_disp_intr* + tu102_disp_init MSK/EN"),
    (0x612200, 0x612300, "head-clk", "gf119.c:419 (0x612200 + head*0x800)"),
    (0x612300, 0x612400, "sor-clk", "ga102.c:61 dp links, gm200.c:107 (0x612300 + sor*0x800)"),
    (0x612400, 0x612500, "sor-pll", "gm200.c:112 (0x612388 + sor*0x80)?"),
    (0x612a00, 0x612c00, "head-clk", "gf119.c (0x612200 + head*0x800)"),
    (0x616000, 0x618000, "head", "head regs (0x616000 + head*0x800): g84/gv100/tu102"),
    (0x61c000, 0x61e000, "sor", "SOR regs (0x61c000 + sor*0x800): g94/gm107/gm200/ga102"),
    (0x620000, 0x640000, "caps-rd", "tu102_disp_init (read side)"),
    (0x640000, 0x641000, "caps", "tu102_disp_init: capabilities copied to 0x640000"),
    (0x680000, 0x690000, "core-put", "gv100.c:734 core user area (PUT at 0x680000)"),
    (0x690000, 0x6a0000, "wndw-put", "gv100.c:333 window user areas (0x690000 + (user-1)*0x1000)"),
    (0x6b0000, 0x6c0000, "wimm-put", "window-immediate user areas"),
    (0x6f0000, 0x700000, "hda", "gv100.c:38 gv100_sor_hda_* (audio ELD)"),
]


def disp_class(o):
    for lo, hi, label, _ in DISP_CLASSES:
        if lo <= o < hi:
            return label
    return None


def disp(d, t0, t1):
    off, base = align(d, quiet=True)
    lo, hi = t0 + off, t1 + off
    for k, t, a, v in iter_trace(d):
        if t > hi:
            break
        if t < lo or k != "W" or not base <= a < base + 0x1000000:
            continue
        o = a - base
        c = disp_class(o)
        if c and not (c == "intr" and o in (0x611800, 0x611804)):  # vblank acks
            print(f"{t - off:.6f} {c:<9} {o:06x} {v:08x}")


def mem(d, lo, hi, t0, t1):
    """Non-zero writes into the physical range [lo, hi) (e.g. a BAR3
    instance-memory mapping from a MAP line), offsets relative to lo."""
    off, _ = align(d, quiet=True)
    for k, t, a, v in iter_trace(d):
        if t > t1 + off:
            break
        if k == "W" and lo <= a < hi and v and t >= t0 + off:
            print(f"{t - off:.6f} {a - lo:#07x} {v:08x}")


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
    elif cmd == "aux":
        if len(argv) > 5:
            aux(d, int(argv[3], 0), argv[4], argv[5])
        else:
            aux(d, int(argv[3], 0))
    elif cmd == "i2c":
        i2c(d, int(argv[3], 0))
    elif cmd == "core":
        if len(argv) > 4:
            core(d, int(argv[3]), argv[4])
        else:
            core(d)
    elif cmd == "disp":
        disp(d, float(argv[3]), float(argv[4]))
    elif cmd == "mem":
        mem(d, int(argv[3], 0), int(argv[4], 0), float(argv[5]), float(argv[6]))
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv)
