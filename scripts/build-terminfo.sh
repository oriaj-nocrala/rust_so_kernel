#!/usr/bin/env bash
# scripts/build-terminfo.sh
#
# First slice of a real ncurses port (see the ncurses/ git submodule):
# compiles a small subset of ncurses' own terminfo database
# (ncurses/misc/terminfo.src) into disk-image-root/usr/share/terminfo/,
# from where the root build.rs seeds/syncs it into disk.img (ext2, mounted
# at /mnt) the same way freedoom1.wad and id1/pak0.pak are. This is data,
# not a program that runs on the kernel target — `tic` (the terminfo
# compiler, ncurses/progs/tic.c) only ever runs on the HOST, exactly once,
# to produce the binary .terminfo entries a future in-kernel libtinfo will
# read via setupterm()/$TERMINFO at runtime. No terminal-capability data is
# hand-written here — every byte in the output comes straight out of
# ncurses' own terminfo.src.
#
# Only a handful of entries are compiled, not the full ~700-terminal
# database (~7MB compiled): this kernel's own framebuffer console spits out
# a "linux"-console-compatible subset of ANSI/SGR (see
# kernel/src/drivers/framebuffer_console.rs's dispatch_csi), so `linux` is
# the one that actually matters; the rest are cheap (36K total for all of
# them) and cover the TERM values a real ssh/tmux session might set.
#
# Not committed to git (see .gitignore) — regenerated on demand, same
# pattern as sysroot/ and disk-image-root/freedoom1.wad.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NCURSES_SRC="$REPO_ROOT/ncurses"
BUILD_DIR="$REPO_ROOT/build-terminfo-host"
DEST="$REPO_ROOT/disk-image-root/usr/share/terminfo"

# Entries this kernel can plausibly need: our own console (linux), the
# lowest common denominator (ansi, dumb), the classic ones real programs
# still probe for (vt100/vt102), and what a real client would set $TERM to
# when attaching over ssh/tmux/screen (xterm*, screen*).
ENTRIES="linux,ansi,xterm,xterm-256color,vt100,vt102,dumb,screen,screen-256color"

if [ -f "$DEST/l/linux" ]; then
    echo "build-terminfo: $DEST already present"
    exit 0
fi

if [ ! -f "$NCURSES_SRC/misc/terminfo.src" ]; then
    echo "error: $NCURSES_SRC/misc/terminfo.src missing — run 'git submodule update --init ncurses'" >&2
    exit 1
fi

for tool in gcc make; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "error: required host tool '$tool' not found in PATH." >&2
        exit 1
    fi
done

TIC_BIN="$BUILD_DIR/progs/tic"

if [ ! -x "$TIC_BIN" ]; then
    echo "build-terminfo: building host 'tic' from the ncurses submodule ..."
    mkdir -p "$BUILD_DIR"
    (
        cd "$BUILD_DIR"
        # This is a HOST build (native gcc, no cross sysroot): tic only
        # ever runs at build time on the machine running `cargo build`, to
        # compile terminfo.src into the binary format libtinfo reads later
        # inside the kernel. --disable-database here would be backwards —
        # this build's whole job IS producing that database.
        # unset TERMINFO/TERMINFO_DIRS: some dev shells (esp. inside
        # terminal emulators that ship their own terminfo, e.g. kitty) set
        # these, and configure would otherwise bake the host's own
        # terminfo path into the summary instead of ncurses' compiled-in
        # default — irrelevant here since we only ever invoke ./tic
        # directly with -o, but worth not depending on.
        env -u TERMINFO -u TERMINFOS \
            "$NCURSES_SRC/configure" \
                --without-shared --without-cxx --without-ada \
                --without-tests --without-manpages --disable-widec \
                >configure.log 2>&1
        make -C ncurses >make-ncurses.log 2>&1
        make -C progs tic >make-progs.log 2>&1
    )
fi

echo "build-terminfo: compiling terminfo entries ($ENTRIES) ..."
mkdir -p "$DEST"
"$TIC_BIN" -x -e "$ENTRIES" -o "$DEST" "$NCURSES_SRC/misc/terminfo.src"

echo "build-terminfo: $DEST ready ($(du -sh "$DEST" | cut -f1))"
