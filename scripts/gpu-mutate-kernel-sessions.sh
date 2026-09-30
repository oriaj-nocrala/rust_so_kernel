#!/bin/bash
# Sabotage of the /dev/nvgpu session logic (G5 layer 1): each mutant of kernel/src/drivers/dev_nvgpu.rs must make nvgpu_sw_test fail.
# Usage: scripts/gpu-mutate-kernel-sessions.sh ORIGINAL_COPY   (the file is restored at the end, even on ^C)
set -u
cd "$(dirname "$0")/.."
F=kernel/src/drivers/dev_nvgpu.rs
ORIG="$1"
trap 'cp "$ORIG" "$F"' EXIT
mutate() { # name, python-expr old, new
    python3 - "$ORIG" "$F" "$2" "$3" <<'PY'
import sys
o=open(sys.argv[1]).read(); a,b=sys.argv[3],sys.argv[4]
assert a in o, "pattern not found: "+a
open(sys.argv[2],'w').write(o.replace(a,b,1))
PY
    touch build.rs
    out=$(timeout 500 scripts/run-abi-suite.sh nvgpu_sw_test 2>&1)
    if echo "$out" | grep -q "0 not clean"; then echo "SURVIVED  $1"; else echo "DETECTED  $1"; fi
}
mutate "every session gets slot 0's VA range" 'nvgpu::hwq::session_va(slot);' 'nvgpu::hwq::session_va(0);'
mutate "INFO reports session-local VRAM use, not the heap's" 'vram_used_all(), self.session.hw' 'dev.vram_used(), self.session.hw'
mutate "a closed session keeps its slot" 'SLOTS.fetch_and(!(1 << self.slot), Ordering::SeqCst);
    }' '}'
mutate "the slot table has 11 entries" '(0..nvgpu::hwq::SESSIONS).find' '(0..nvgpu::hwq::SESSIONS - 1).find'
