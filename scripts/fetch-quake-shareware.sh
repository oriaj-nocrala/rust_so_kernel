#!/usr/bin/env bash
# scripts/fetch-quake-shareware.sh
#
# Downloads id Software's Quake shareware episode (freely redistributable —
# id Software's own long-standing shareware policy, same legal footing as
# Doom's shareware WAD already used for disk-image-root/freedoom1.wad) and
# extracts pak0.pak to disk-image-root/id1/pak0.pak, if it isn't already
# there. From there the workspace-root build.rs seeds it into disk.img
# (ext2), and Quake reads it at runtime from /mnt/id1/pak0.pak — same
# "read straight off ext2" path DOOM already validated for freedoom1.wad.
#
# Source: an archive.org mirror of the original quake_pak.zip release,
# containing both pak0.pak (shareware — what we want) and pak1.pak (the
# registered/full game's data — NOT extracted or distributed here, only
# pak0.pak is; see quakegeneric_constanos.c / kernel/build.rs for what
# actually reads it).
#
# Not committed to git (~18MB, a large external data asset rather than
# source — see .gitignore) — fetched on demand, same idea as
# scripts/fetch-freedoom.sh.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST_DIR="$REPO_ROOT/disk-image-root/id1"
DEST="$DEST_DIR/pak0.pak"

if [ -f "$DEST" ]; then
    echo "fetch-quake-shareware: $DEST already present"
    exit 0
fi

for tool in curl unzip; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "error: required tool '$tool' not found in PATH." >&2
        exit 1
    fi
done

URL="https://archive.org/download/quake_pak_202306/quake_pak.zip"
TMPZIP="$(mktemp -t quake-pak-XXXXXX.zip)"
trap 'rm -f "$TMPZIP"' EXIT

echo "fetch-quake-shareware: downloading $URL ..."
curl -L -o "$TMPZIP" "$URL"

mkdir -p "$DEST_DIR"
unzip -j -o "$TMPZIP" 'pak0.pak' -d "$DEST_DIR"

echo "fetch-quake-shareware: $DEST ready"
