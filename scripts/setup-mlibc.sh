#!/usr/bin/env bash
# scripts/setup-mlibc.sh
#
# Rebuilds sysroot/ (crt1.o + libc.a + headers) from the mlibc git submodule.
#
# The mlibc submodule (.gitmodules) points at upstream managarm/mlibc, which
# has no support for this kernel's syscall ABI. mlibc-port/constanos-sysdeps/
# in this repo holds our own out-of-tree sysdeps port (see mlibc-port/README
# if present); this script copies it into the submodule checkout and
# registers it in mlibc's meson.build before building, so everything is
# reproducible from a fresh `git clone` + `git submodule update --init`
# without needing a fork of mlibc.
#
# Idempotent: safe to re-run; only rebuilds what changed.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

if [ ! -f mlibc/meson.build ]; then
    echo "setup-mlibc: mlibc/ submodule not checked out yet — initializing..."
    git submodule update --init --recursive
fi

# ── 1. Copy our sysdeps port into the submodule checkout ──────────────────

rm -rf mlibc/sysdeps/constanos
cp -r mlibc-port/constanos-sysdeps mlibc/sysdeps/constanos

# ── 2. Register 'constanos' in mlibc/meson.build (idempotent) ─────────────

if ! grep -q "host_machine.system() == 'constanos'" mlibc/meson.build; then
    python3 - "$REPO_ROOT/mlibc/meson.build" <<'PYEOF'
import sys

path = sys.argv[1]
with open(path) as f:
    content = f.read()

marker = "else\n\terror('No sysdeps defined for OS: ' + host_machine.system())"
if marker not in content:
    print("error: could not find sysdeps if/elif chain terminator in mlibc/meson.build "
          "(upstream mlibc layout may have changed) -- setup-mlibc.sh needs updating",
          file=sys.stderr)
    sys.exit(1)

insertion = (
    "elif host_machine.system() == 'constanos'\n"
    "\trtld_include_dirs += include_directories('sysdeps/constanos/include')\n"
    "\tlibc_include_dirs += include_directories('sysdeps/constanos/include')\n"
    "\tsubdir('sysdeps/constanos')\n"
    + marker
)
content = content.replace(marker, insertion, 1)

with open(path, "w") as f:
    f.write(content)
PYEOF
    echo "setup-mlibc: registered 'constanos' in mlibc/meson.build"
fi

# ── 3. Configure + build + install into sysroot/ ───────────────────────────

for tool in meson ninja clang clang++ llvm-ar llvm-strip llvm-objcopy; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "error: required build tool '$tool' not found in PATH." >&2
        echo "  on Arch: sudo pacman -S --needed clang llvm meson ninja lld" >&2
        exit 1
    fi
done

if [ ! -d build-mlibc ]; then
    meson setup build-mlibc mlibc \
        --cross-file mlibc-cross.ini \
        --prefix=/usr \
        -Ddefault_library=static \
        -Dlibgcc_dependency=false \
        -Dbuild_tests=false
fi

DESTDIR="$REPO_ROOT/sysroot" ninja -C build-mlibc install

echo "setup-mlibc: sysroot ready at $REPO_ROOT/sysroot"
