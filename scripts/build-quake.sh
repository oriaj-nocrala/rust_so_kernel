#!/usr/bin/env bash
# scripts/build-quake.sh
#
# Cross-compiles quakegeneric (git submodule, upstream erysdren/quakegeneric
# — a doomgeneric-style minimal port of id Software's GPL WinQuake source)
# plus our own platform port (quake-port/quakegeneric_constanos.c) against
# sysroot/, dropping the resulting static ELF at kernel/embedded/quake.elf.
#
# Same shape as scripts/build-doom.sh: quakegeneric has no config step
# either (its own makefile just lists every source file), so this hardcodes
# that same file list — read straight from quakegeneric/source/makefile's
# own OBJS variable — minus its own platform port file
# (quakegeneric_sdl2.c), plus ours.
#
# Idempotent: safe to re-run; always rebuilds (a from-scratch compile of
# ~64 files is a few seconds, not worth an incremental build here).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

if [ ! -f sysroot/usr/lib/libc.a ]; then
    echo "build-quake: sysroot/ missing — run scripts/setup-mlibc.sh first" >&2
    exit 1
fi

if [ ! -f quakegeneric/source/quakegeneric.h ]; then
    echo "build-quake: quakegeneric/ submodule not checked out yet — initializing..." >&2
    git submodule update --init quakegeneric
fi

for tool in clang llvm-ar; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "error: required build tool '$tool' not found in PATH." >&2
        exit 1
    fi
done

QG_SRC="$REPO_ROOT/quakegeneric/source"

# ── Source list ─────────────────────────────────────────────────────────
#
# Every *.c quakegeneric/source/makefile's own OBJS variable compiles,
# minus its platform file (quakegeneric_sdl2.c) — same file set, just
# swapping the platform port for ours.
CORE_SOURCES=(
    cd_null.c chase.c cl_demo.c cl_input.c cl_main.c cl_parse.c cl_tent.c
    cmd.c common.c console.c crc.c cvar.c d_edge.c d_fill.c d_init.c
    d_modech.c d_part.c d_polyse.c d_scan.c d_sky.c d_sprite.c d_surf.c
    d_vars.c d_zpoint.c draw.c host_cmd.c host.c in_null.c keys.c
    mathlib.c menu.c model.c net_loop.c net_main.c net_none.c net_vcr.c
    nonintel.c pr_cmds.c pr_edict.c pr_exec.c r_aclip.c r_alias.c r_bsp.c
    r_draw.c r_edge.c r_efrag.c r_light.c r_main.c r_misc.c r_part.c
    r_sky.c r_sprite.c r_surf.c r_vars.c sbar.c screen.c snd_null.c
    sv_main.c sv_move.c sv_phys.c sv_user.c sys_null.c vid_null.c view.c
    wad.c world.c zone.c quakegeneric.c
)

SOURCES=()
for f in "${CORE_SOURCES[@]}"; do
    SOURCES+=("$QG_SRC/$f")
done
SOURCES+=("$REPO_ROOT/quake-port/quakegeneric_constanos.c")

# ── Cross-compiler wrapper ──────────────────────────────────────────────
#
# Same technique as scripts/build-doom.sh: no real cross toolchain with
# crt/libc wired in via a --sysroot spec, so inject crt1.o/libc.a/-static
# ourselves at the single final-link invocation.
mkdir -p build-quake
CC_WRAPPER="$REPO_ROOT/build-quake/cc-wrapper.sh"
cat > "$CC_WRAPPER" <<WRAPEOF
#!/usr/bin/env bash
set -e
SYSROOT="$REPO_ROOT/sysroot"
RESOURCE_INC="\$(clang --print-resource-dir)/include"

exec clang \\
    --target=x86_64-constanos-elf \\
    -ffreestanding \\
    -fno-stack-protector \\
    -fomit-frame-pointer \\
    -mno-red-zone \\
    -D_GNU_SOURCE \\
    -nostdinc \\
    -isystem "\$SYSROOT/usr/include" \\
    -isystem "\$RESOURCE_INC" \\
    -static -nostdlib \\
    -Wno-implicit-function-declaration \\
    "\$SYSROOT/usr/lib/crt1.o" \\
    "\$@" \\
    "\$SYSROOT/usr/lib/libc.a"
WRAPEOF
chmod +x "$CC_WRAPPER"

# ── Build ────────────────────────────────────────────────────────────────

mkdir -p kernel/embedded
"$CC_WRAPPER" -I"$QG_SRC" -O2 "${SOURCES[@]}" -o kernel/embedded/quake.elf

echo "build-quake: kernel/embedded/quake.elf ready"
