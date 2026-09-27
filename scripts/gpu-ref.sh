#!/usr/bin/env bash
# scripts/gpu-ref.sh — clone the pinned references for the GPU plan
# (docs/gpu/gpu-plan.md, "Referencias fijadas") outside the repo.
#
#   scripts/gpu-ref.sh [GSP_VERSION]     default GSP_VERSION: 570.144
#
# Into $GPU_REF (default ~/src/gpu-ref):
#   linux/                  sparse, shallow: drivers/gpu/drm/nouveau, drivers/gpu/nova-core,
#                           drivers/gpu/drm/display (DP helpers), include/drm, include/uapi/drm
#                           — at the tag of the host's running kernel
#   open-gpu-kernel-modules/ shallow, at GSP_VERSION (the RM ABI source of truth)
#   envytools/              shallow, master (rnndb + demmio, to read mmiotrace traces)
#
# Idempotent: an existing clone is left alone. Every constant in the driver
# cites file:line in one of these trees, so the tags are recorded in
# $GPU_REF/PINNED and must match the plan.
set -euo pipefail

GSP_VERSION="${1:-570.144}"
GPU_REF="${GPU_REF:-$HOME/src/gpu-ref}"
# 7.2.2-zen1-1-zen -> v7.2.2 (the zen patchset doesn't touch nouveau).
LINUX_TAG="v$(uname -r | sed -E 's/^([0-9]+\.[0-9]+(\.[0-9]+)?).*/\1/')"

mkdir -p "$GPU_REF"
cd "$GPU_REF"

if [[ ! -d linux ]]; then
    # Stable tags (v7.2.2) live in the stable tree, not in torvalds/linux.
    git clone --depth 1 --filter=blob:none --sparse --branch "$LINUX_TAG" \
        https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git linux
    git -C linux sparse-checkout set \
        drivers/gpu/drm/nouveau drivers/gpu/nova-core drivers/gpu/drm/display \
        include/drm include/uapi/drm
fi

if [[ ! -d open-gpu-kernel-modules ]]; then
    git clone --depth 1 --branch "$GSP_VERSION" \
        https://github.com/NVIDIA/open-gpu-kernel-modules.git
fi

if [[ ! -d envytools ]]; then
    git clone --depth 1 https://github.com/envytools/envytools.git
fi

{
    echo "linux                   $LINUX_TAG $(git -C linux rev-parse HEAD)"
    echo "open-gpu-kernel-modules $GSP_VERSION $(git -C open-gpu-kernel-modules rev-parse HEAD)"
    echo "envytools               master $(git -C envytools rev-parse HEAD)"
} > PINNED
cat PINNED
