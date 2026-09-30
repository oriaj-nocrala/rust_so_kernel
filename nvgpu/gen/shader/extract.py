#!/usr/bin/env python3
"""Extract a kernel's SASS and metadata from a cubin (ELF64) built by nvcc/ptxas.

    extract.py fill.cubin fill OUT.bin

Writes the raw bytes of `.text.<kernel>` to OUT.bin and prints, one per line: the kernel's register count
(SHI_REGISTERS), the size of `.nv.constant0.<kernel>`, and the code size.
"""
import struct
import sys

cubin, kernel, out = sys.argv[1:4]
d = open(cubin, "rb").read()
assert d[:4] == b"\x7fELF" and d[4] == 2, "not an ELF64"
shoff, = struct.unpack_from("<Q", d, 0x28)
shentsize, shnum, shstrndx = struct.unpack_from("<HHH", d, 0x3A)

def sh(i):
    name, typ, flags, addr, off, size, link, info, align, entsize = struct.unpack_from("<IIQQQQIIQQ", d, shoff + i * shentsize)
    return dict(name=name, type=typ, off=off, size=size, info=info)

secs = [sh(i) for i in range(shnum)]
strtab = secs[shstrndx]
def name_of(s):
    o = strtab["off"] + s["name"]
    return d[o:d.index(b"\0", o)].decode()

by = {name_of(s): s for s in secs}
text = by[".text." + kernel]
code = d[text["off"]:text["off"] + text["size"]]
open(out, "wb").write(code)

# SHI_REGISTERS (what nvdisasm prints as `.sectioninfo SHI_REGISTERS=n`) is the top byte of the code section's sh_info
regs = text["info"] >> 24
print("registers", regs)
print("constant0", by[".nv.constant0." + kernel]["size"])
print("code", len(code))
