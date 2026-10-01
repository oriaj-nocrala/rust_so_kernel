#!/bin/bash
# Put the constanos changes on a Mesa checkout (main 20f48abe; ~/src/gpu-ref/mesa by default): the tracked-file patch, the new
# nvkmd/constanos files, and the /dev/nvgpu interface and window headers from this tree. Safe to repeat.
#   mesa-port/apply.sh [MESA_DIR]
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
mesa="${1:-$HOME/src/gpu-ref/mesa}"
[ -d "$mesa/src/nouveau/vulkan" ] || { echo "no Mesa checkout at $mesa" >&2; exit 1; }
if git -C "$mesa" apply --check "$here/patches/0001-nvk-constanos.patch" 2>/dev/null; then
    git -C "$mesa" apply "$here/patches/0001-nvk-constanos.patch"
    echo "applied 0001-nvk-constanos.patch"
elif git -C "$mesa" apply --check -R "$here/patches/0001-nvk-constanos.patch" 2>/dev/null; then
    echo "0001-nvk-constanos.patch is already applied"
else
    echo "the patch neither applies nor is applied: is $mesa at 20f48abe?" >&2; exit 1
fi
mkdir -p "$mesa/src/nouveau/vulkan/nvkmd/constanos"
cp "$here"/overlay/src/nouveau/vulkan/nvkmd/constanos/nvkmd_constanos.[ch] "$mesa/src/nouveau/vulkan/nvkmd/constanos/"
cp "$repo/nvgpu/uapi/nvgpu.h" "$mesa/src/nouveau/vulkan/nvkmd/constanos/nvgpu.h"
cp "$repo/userspace/c/include/constanos_vk_window.h" "$mesa/src/vulkan/wsi/constanos_window.h"
echo "overlay copied"
