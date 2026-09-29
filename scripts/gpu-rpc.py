#!/usr/bin/env python3
"""scripts/gpu-rpc.py — the GSP-RM RPCs nouveau sent and received, from a trace's dmesg.

The oracle traces (`~/constanos-gpu-oracle/trace-gsp/dmesg.txt`) were taken with
`debug=gsp=trace`, so every RPC nouveau sent (`gsp: rpc fn:N len:..` + `rpc: 0000000: ..`
hexdump of the payload) and every message it received (`gsp:msg fn:N ..` + `msg: ..`) is there.

  gpu-rpc.py list  DIR [T0 T1] [--fn N]   one row per sent RPC, decoded: time, index, function,
                                          for RM_ALLOC (103) client/parent/object/class/params,
                                          for RM_CONTROL (76) client/object/cmd/params
  gpu-rpc.py recv  DIR [T0 T1]            one row per received message (function, length)
  gpu-rpc.py dump  DIR INDEX OUT_PREFIX   write OUT_PREFIX-req.bin (the sent payload) and
                                          OUT_PREFIX-rep.bin (the next received message of the
                                          same function, if any); INDEX = the `list` index
  gpu-rpc.py text  DIR T0 T1              the dmesg lines of nouveau's own debug output in the window
                                          (`gsp: cli:0x.. obj:0x.. new obj`, `ctrl cmd:`, `seq ...`)

Payload = what follows the 32-byte RPC header. ALLOC header (32 B): hClient, hParent, hObject,
hClass, status, paramsSize, flags, reserved; CONTROL header (24 B): hClient, hObject, cmd, status,
paramsSize, flags. Times are dmesg seconds (mmiotrace is +0.1229 s in trace-gsp).
"""
import re, struct, sys

def load(d):
    return open(d.rstrip('/') + '/dmesg.txt', errors='replace').read().split('\n')

def blocks(lines, kind):
    """(time, fn, payload bytes) of every `rpc fn:` (sent) or `gsp:msg fn:` (received) block."""
    head = r'gsp: rpc fn:(\d+) len' if kind == 'rpc' else r'gsp:msg fn:(\d+) len'
    tag = 'rpc' if kind == 'rpc' else 'msg'
    out, i = [], 0
    while i < len(lines):
        m = re.search(head, lines[i])
        if not m:
            i += 1
            continue
        data, j = bytearray(), i + 1
        while j < len(lines):
            mm = re.search(r'%s: ([0-9a-f]{8}): ((?:[0-9a-f]{2} )+)' % tag, lines[j])
            if not mm:
                break
            data += bytes.fromhex(mm.group(2).replace(' ', ''))
            j += 1
        t = float(re.match(r'\[\s*([0-9.]+)\]', lines[i]).group(1))
        out.append((t, int(m.group(1)), bytes(data)))
        i = j
    return out

def describe(fn, b):
    if fn == 103 and len(b) >= 32:
        c, p, o, cl, st, ps = struct.unpack('<6I', b[:24])
        return f"ALLOC client={c:#010x} parent={p:#010x} obj={o:#010x} class={cl:#06x} params={ps}"
    if fn == 76 and len(b) >= 24:
        c, o, cmd, st, ps = struct.unpack('<5I', b[:20])
        return f"CONTROL client={c:#010x} obj={o:#010x} cmd={cmd:#010x} params={ps}"
    if fn == 10 and len(b) >= 16:
        c, p, o, st = struct.unpack('<4I', b[:16])
        return f"FREE client={c:#010x} obj={o:#010x}"
    return f"payload {len(b)} B"

def window(args):
    nums = [a for a in args if re.fullmatch(r'[0-9.]+', a)]
    return (float(nums[0]), float(nums[1])) if len(nums) >= 2 else (0.0, 1e9)

def main(argv):
    if len(argv) < 3:
        sys.exit(__doc__)
    cmd, d = argv[1], argv[2]
    lines = load(d)
    fnf = int(argv[argv.index('--fn') + 1]) if '--fn' in argv else None
    if cmd == 'list':
        t0, t1 = window([a for a in argv[3:] if a != '--fn' and not (fnf is not None and a == str(fnf))])
        for idx, (t, fn, b) in enumerate(blocks(lines, 'rpc')):
            if t0 <= t <= t1 and (fnf is None or fn == fnf):
                print(f"{t:9.4f} #{idx:<4d} fn{fn:<4d} {describe(fn, b)}")
    elif cmd == 'recv':
        t0, t1 = window(argv[3:])
        for idx, (t, fn, b) in enumerate(blocks(lines, 'msg')):
            if t0 <= t <= t1:
                print(f"{t:9.4f} #{idx:<4d} fn{fn:<5d} {describe(fn, b)}")
    elif cmd == 'dump':
        idx, prefix = int(argv[3]), argv[4]
        sent = blocks(lines, 'rpc')
        t, fn, b = sent[idx]
        open(prefix + '-req.bin', 'wb').write(b)
        rep = next(((tt, bb) for tt, f, bb in blocks(lines, 'msg') if f == fn and tt >= t), None)
        if rep:
            open(prefix + '-rep.bin', 'wb').write(rep[1])
        print(f"fn{fn} at {t}: request {len(b)} B" + (f", reply {len(rep[1])} B at {rep[0]}" if rep else ", no reply found"))
    elif cmd == 'text':
        t0, t1 = window(argv[3:])
        for l in lines:
            m = re.match(r'\[\s*([0-9.]+)\] nouveau .*?gsp: (.*)$', l)
            if m and t0 <= float(m.group(1)) <= t1 and not m.group(2).startswith(('rpc:', 'msg:')):
                print(l[:200])
    else:
        sys.exit(__doc__)

if __name__ == '__main__':
    main(sys.argv)
