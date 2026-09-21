#!/usr/bin/env bash
# scripts/build-libtinfo.sh
#
# Second slice of the ncurses port (see scripts/build-terminfo.sh for the
# first — the terminfo database itself). Cross-compiles ncurses' `tinfo`
# AND `ncurses` libraries (setupterm/tgetent/tigetstr, plus the real
# curses windowing API — initscr/refresh/addwstr/etc.) against sysroot/,
# static, for this kernel's x86_64-constanos-elf target. Drops the
# result at build-libtinfo/prefix/lib/lib{tinfo,ncurses}w.a +
# build-libtinfo/prefix/include/ncursesw/ (a scratch install prefix, not
# copied anywhere yet — nothing links against it until a test program
# does).
#
# Wide-char (--enable-widec, giving the 'w'-suffixed libs/headers) rather
# than the narrower default: cmatrix's normal (non-lambda) character path
# renders through addwstr()/wchar_t unconditionally (see cmatrix.c's
# matrix-drawing loop) — there's no ASCII-only fallback path, so the
# plain (non-wide) libncurses.a wouldn't satisfy it at all.
#
# Deliberately NOT --disable-database: unlike a typical embedded port that
# bakes in one hardcoded terminal entry, this reads the real terminfo tree
# scripts/build-terminfo.sh ships to /mnt/usr/share/terminfo, via
# --with-terminfo-dirs/--with-default-terminfo-dir pointing there — the
# path this kernel's ext2 mount actually uses.
#
# This is genuinely a cross-compile (unlike build-terminfo.sh's `tic`,
# which only ever runs on the host): ncurses' own build needs a few small
# C programs (make_hash, make_keys, ...) built for and run on the BUILD
# machine to generate tables consumed by the TARGET build — that's what
# --with-build-cc et al. below are for.
#
# Requires sysroot/ (scripts/setup-mlibc.sh) and the ncurses/ submodule.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

if [ ! -f sysroot/usr/lib/libc.a ]; then
    echo "build-libtinfo: sysroot/ missing — run scripts/setup-mlibc.sh first" >&2
    exit 1
fi

if [ ! -f ncurses/configure ]; then
    echo "build-libtinfo: ncurses/ submodule not checked out yet — initializing..."
    git submodule update --init ncurses
fi

for tool in clang llvm-ar llvm-ranlib cc; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "error: required build tool '$tool' not found in PATH." >&2
        exit 1
    fi
done

BUILD_DIR="$REPO_ROOT/build-libtinfo"
PREFIX="$BUILD_DIR/prefix"
mkdir -p "$BUILD_DIR"

# ── 1. Cross-compiler wrapper ───────────────────────────────────────────────
#
# Same shape as build-busybox.sh's wrapper (see its own comment for the
# is_link/-r rationale) — duplicated rather than shared because each of
# these port scripts is meant to be self-contained and independently
# readable, same convention as build-doom.sh/build-quake.sh.
CC_WRAPPER="$BUILD_DIR/cc-wrapper.sh"
cat > "$CC_WRAPPER" <<WRAPEOF
#!/usr/bin/env bash
set -e
SYSROOT="$REPO_ROOT/sysroot"
RESOURCE_INC="\$(clang --print-resource-dir)/include"

is_link=1
for arg in "\$@"; do
    case "\$arg" in
        -c|-E|-S|-r) is_link=0 ;;
    esac
done

COMMON=(
    --target=x86_64-constanos-elf
    -ffreestanding
    -fno-stack-protector
    -fomit-frame-pointer
    -mno-red-zone
    -D_GNU_SOURCE
    -nostdinc
    -isystem "\$SYSROOT/usr/include"
    -isystem "\$RESOURCE_INC"
)

if [ "\$is_link" = "1" ]; then
    args=()
    for arg in "\$@"; do
        case "\$arg" in
            -lm|-lrt) continue ;;
            *) args+=("\$arg") ;;
        esac
    done
    exec clang "\${COMMON[@]}" -static -nostdlib \\
        "\$SYSROOT/usr/lib/crt1.o" \\
        "\${args[@]}" \\
        "\$SYSROOT/usr/lib/libc.a"
else
    exec clang "\${COMMON[@]}" "\$@"
fi
WRAPEOF
chmod +x "$CC_WRAPPER"

# ── 2. Configure ─────────────────────────────────────────────────────────
#
# --host tells autoconf this is a cross build (feature probes switch from
# TRY_RUN to TRY_COMPILE/TRY_LINK automatically); --with-build-cc et al.
# give it a *working native* compiler for the handful of table-generator
# programs (make_hash, make_keys — see ncurses/progs/) that must run
# during the build itself, on this machine, not the target.
(
    cd "$BUILD_DIR"
    env CC="$CC_WRAPPER" AR=llvm-ar RANLIB=llvm-ranlib \
        "$REPO_ROOT/ncurses/configure" \
            --host=x86_64-constanos-elf \
            --with-build-cc=cc \
            --with-build-cpp="cc -E" \
            --prefix="$PREFIX" \
            --without-shared --without-cxx --without-ada \
            --without-tests --without-manpages --without-progs \
            --enable-widec --with-termlib \
            --with-terminfo-dirs=/mnt/usr/share/terminfo \
            --with-default-terminfo-dir=/mnt/usr/share/terminfo \
            --without-develop \
        >configure.log 2>&1
)

# ── 3. Build + install (to the scratch prefix, not the real sysroot) ──────

make -C "$BUILD_DIR" -j"$(nproc)" >"$BUILD_DIR/make.log" 2>&1
make -C "$BUILD_DIR" install.libs install.includes >"$BUILD_DIR/install.log" 2>&1

echo "build-libtinfo: $PREFIX/lib/libtinfow.a ready"
