#!/usr/bin/env bash
# scripts/fetch-fonts.sh
#
# Downloads Noto Sans and Noto Sans Mono, Regular and Bold (SIL OFL 1.1,
# freely redistributable — the licence is copied next to them) from pinned
# notofonts releases, checks each archive's sha256, and extracts the
# unhinted TTFs to disk-image-root/usr/share/fonts/. From there the root
# build.rs syncs them onto disk.img, and programs read them at runtime from
# /mnt/usr/share/fonts/. The `text` crate's host tests read them from
# disk-image-root/ too (docs/gui/text-plan.md).
#
# Unhinted: `swash` rasterises without the TrueType bytecode interpreter
# here, so the hinted builds' instructions would only cost bytes.
#
# Not committed to git (external data, ~1.6 MB) — fetched on demand, like
# fetch-freedoom.sh. Idempotent: does nothing if all four files are there.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$REPO_ROOT/disk-image-root/usr/share/fonts"
BASE="https://github.com/notofonts/latin-greek-cyrillic/releases/download"

# release tag, sha256 of its zip
RELEASES=(
    "NotoSans-v2.015 0c34df072a3fa7efbb7cbf34950e1f971a4447cffe365d3a359e2d4089b958f5"
    "NotoSansMono-v2.014 090cf6c5e03f337a755630ca888b1fef463e64ae7b33ee134e9309c05f978732"
)

have_all=1
for f in NotoSans-Regular NotoSans-Bold NotoSansMono-Regular NotoSansMono-Bold; do
    [ -f "$DEST/$f.ttf" ] || have_all=0
done
if [ "$have_all" = 1 ] && [ -f "$DEST/OFL.txt" ]; then
    echo "fetch-fonts: $DEST already present"
    exit 0
fi

for tool in curl unzip sha256sum; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "error: required tool '$tool' not found in PATH." >&2
        exit 1
    fi
done

TMP="$(mktemp -d -t fetch-fonts-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$DEST"

for entry in "${RELEASES[@]}"; do
    read -r tag sum <<<"$entry"
    family="${tag%-v*}"
    zip="$TMP/$tag.zip"
    echo "fetch-fonts: downloading $tag ..."
    curl -fL -o "$zip" "$BASE/$tag/$tag.zip"
    echo "$sum  $zip" | sha256sum -c --quiet - || {
        echo "error: $tag.zip does not match its pinned sha256" >&2
        exit 1
    }
    for style in Regular Bold; do
        unzip -j -o -q "$zip" "$family/unhinted/ttf/$family-$style.ttf" -d "$DEST"
    done
    unzip -j -o -q "$zip" OFL.txt -d "$DEST"
done

echo "fetch-fonts: $DEST ready"
ls -l "$DEST"
