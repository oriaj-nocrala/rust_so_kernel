#!/usr/bin/env bash
# scripts/fetch-freedoom.sh
#
# Downloads Freedoom (BSD-3-Clause-style license, freely redistributable —
# see its own COPYING.txt) and extracts freedoom1.wad (the smaller "Phase
# 1" episode) to disk-image-root/freedoom1.wad, if it isn't already there.
# From there kernel/build.rs copies it to kernel/embedded/freedoom1.wad,
# where drivers/dev_wad.rs include_bytes!'s it and serves it as
# /dev/freedoom1.wad — the path DOOM actually loads it from. It's also
# still baked into disk.img (mke2fs -d disk-image-root) and visible at
# /mnt/freedoom1.wad, but that route is NOT used by DOOM anymore: its
# access pattern triggered transient ATA read corruption (see
# dev_wad.rs's header comment).
#
# Not committed to git (~29MB, a large external data asset rather than
# source — see .gitignore) — fetched on demand, same idea as sysroot/
# and the busybox/ submodule, just without needing a submodule for a
# single static file.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$REPO_ROOT/disk-image-root/freedoom1.wad"

if [ -f "$DEST" ]; then
    echo "fetch-freedoom: $DEST already present"
    exit 0
fi

for tool in curl unzip; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "error: required tool '$tool' not found in PATH." >&2
        exit 1
    fi
done

URL="https://github.com/freedoom/freedoom/releases/download/v0.13.0/freedoom-0.13.0.zip"
TMPZIP="$(mktemp -t freedoom-XXXXXX.zip)"
trap 'rm -f "$TMPZIP"' EXIT

echo "fetch-freedoom: downloading $URL ..."
curl -L -o "$TMPZIP" "$URL"

mkdir -p "$REPO_ROOT/disk-image-root"
unzip -j -o "$TMPZIP" 'freedoom-0.13.0/freedoom1.wad' -d "$REPO_ROOT/disk-image-root"

echo "fetch-freedoom: $DEST ready"
