#!/usr/bin/env bash
# scripts/build-doom.sh
#
# Cross-compiles doomgeneric (git submodule, upstream ozkl/doomgeneric) plus
# our own platform port (doom-port/doomgeneric_constanos.c) against
# sysroot/, dropping the resulting static ELF at kernel/embedded/doom.elf.
#
# Requires sysroot/ to already exist — run scripts/setup-mlibc.sh (or just
# `cargo build`, which does it automatically) first.
#
# Unlike BusyBox, doomgeneric has no Kconfig/config step: it ships a
# static config.h and no Makefile at all (only a Visual Studio .sln), so
# this script hardcodes the same source file list as that .sln's own
# Win32 build (`doomgeneric.vcxproj`) minus its platform file
# (doomgeneric_win.c) and the SDL/Allegro sound backends (unused — this
# kernel has its own AC97 driver + sound_module_t instead, see
# doom-port/doomgeneric_sound_constanos.c), plus our own
# doomgeneric_constanos.c and doomgeneric_sound_constanos.c.
#
# FEATURE_SOUND is defined below so i_sound.c actually registers
# DG_sound_module instead of behaving as a null backend.
#
# Idempotent: safe to re-run; always rebuilds (no incremental object
# cache — a from-scratch compile of ~83 files is a few seconds, not worth
# the complexity of a real incremental build here).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

if [ ! -f sysroot/usr/lib/libc.a ]; then
    echo "build-doom: sysroot/ missing — run scripts/setup-mlibc.sh first" >&2
    exit 1
fi

if [ ! -f doomgeneric/doomgeneric/doomgeneric.h ]; then
    echo "build-doom: doomgeneric/ submodule not checked out yet — initializing..." >&2
    git submodule update --init doomgeneric
fi

for tool in clang llvm-ar; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "error: required build tool '$tool' not found in PATH." >&2
        exit 1
    fi
done

DG_SRC="$REPO_ROOT/doomgeneric/doomgeneric"

# ── Source list ─────────────────────────────────────────────────────────
#
# Every *.c the upstream Win32 build (doomgeneric.vcxproj) compiles,
# minus its platform file — same file set, just swapping the platform
# port for ours.
CORE_SOURCES=(
    am_map.c d_event.c d_items.c d_iwad.c d_loop.c d_main.c d_mode.c d_net.c
    doomdef.c doomgeneric.c doomstat.c dstrings.c dummy.c f_finale.c f_wipe.c
    g_game.c gusconf.c hu_lib.c hu_stuff.c i_cdmus.c icon.c i_endoom.c
    i_input.c i_joystick.c info.c i_scale.c i_sound.c i_system.c i_timer.c
    i_video.c m_argv.c m_bbox.c m_cheat.c m_config.c m_controls.c memio.c
    m_fixed.c m_menu.c m_misc.c m_random.c p_ceilng.c p_doors.c p_enemy.c
    p_floor.c p_inter.c p_lights.c p_map.c p_maputl.c p_mobj.c p_plats.c
    p_pspr.c p_saveg.c p_setup.c p_sight.c p_spec.c p_switch.c p_telept.c
    p_tick.c p_user.c r_bsp.c r_data.c r_draw.c r_main.c r_plane.c r_segs.c
    r_sky.c r_things.c sha1.c sounds.c s_sound.c statdump.c st_lib.c
    st_stuff.c tables.c v_video.c w_checksum.c w_file.c w_file_stdc.c
    wi_stuff.c w_main.c w_wad.c z_zone.c
)

SOURCES=()
for f in "${CORE_SOURCES[@]}"; do
    SOURCES+=("$DG_SRC/$f")
done
SOURCES+=("$REPO_ROOT/doom-port/doomgeneric_constanos.c")
SOURCES+=("$REPO_ROOT/doom-port/doomgeneric_sound_constanos.c")

# ── Cross-compiler wrapper ──────────────────────────────────────────────
#
# Same technique as scripts/build-busybox.sh's cc-wrapper: our target has
# no real cross toolchain with crt/libc wired in via a --sysroot spec, so
# inject crt1.o/libc.a/-static ourselves — but only at the final link, a
# single `clang *.c -o doom.elf` invocation here (no per-file `-c` object
# step, no partial `-r` links to avoid splicing crt1.o into), so there's
# no "only for the real final link" distinction to get wrong here.
mkdir -p build-doom
CC_WRAPPER="$REPO_ROOT/build-doom/cc-wrapper.sh"
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
    -DFEATURE_SOUND \\
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
"$CC_WRAPPER" -I"$DG_SRC" -I"$REPO_ROOT/doom-port/stub-include" -O2 "${SOURCES[@]}" -o kernel/embedded/doom.elf

echo "build-doom: kernel/embedded/doom.elf ready"
