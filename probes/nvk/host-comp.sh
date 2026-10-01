#!/bin/sh
# Builds host_comp.c (the GPU compositor's renderer on the host's Vulkan, against a CPU reference) and runs it.
#   probes/nvk/host-comp.sh [dump-dir]      the frames as PPM in dump-dir
set -e
here=$(cd "$(dirname "$0")" && pwd)
root=$here/../..
out=${1:-/tmp/host-comp}
mkdir -p "$out"
(cd "$root/gui-capi" && cargo build --lib >/dev/null)
${CC:-clang} -O1 -g -Wall -Wextra -Wno-unused-parameter -Wno-unused-function -DCOMP_HOST -I"$here" -I"$root/gui-capi/include" -I"$root/userspace/c/include" \
    "$here/host_comp.c" "$root/gui-capi/target/debug/libgui_capi.a" -o "$out/host-comp" -lvulkan -lpthread -ldl -lm
"$out/host-comp" "$out"
