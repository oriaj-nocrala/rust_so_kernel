#!/bin/sh
# Regenerates snake3d_spv.h from snake3d.vert / snake3d.frag (glslangValidator -V), in the format of tri_spv.h.
set -e
cd "$(dirname "$0")"
t=$(mktemp -d)
glslangValidator -V snake3d.vert -o "$t/vert.spv" >/dev/null
glslangValidator -V snake3d.frag -o "$t/frag.spv" >/dev/null
python3 - "$t" <<'PY' > snake3d_spv.h
import sys
print("/* Generated from snake3d.vert and snake3d.frag with glslangValidator -V (gen-spv.sh): do not edit. */")
for n in ("vert", "frag"):
    d = open(sys.argv[1] + "/%s.spv" % n, "rb").read()
    print("static const unsigned char snake3d_%s_spv[] = {" % n)
    for i in range(0, len(d), 12):
        print("  " + ", ".join("0x%02x" % b for b in d[i:i + 12]) + ",")
    print("};")
    print("static const unsigned int snake3d_%s_spv_len = %d;" % (n, len(d)))
PY
rm -rf "$t"
