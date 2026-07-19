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

# ── 1b. Patch a real upstream mlibc bug: suppressed scanf conversions ─────
#
# `do_scanf` (options/ansi/generic/stdio.cpp) only advances its internal
# `count` inside the `if(typed_dest)` branch of `append_to_buffer` — for a
# suppressed conversion (`%*s`/`%*c`/`%*[`, where `dest` is deliberately
# null), `count` never moves, so the very next `NOMATCH_CHECK(count == 0)`
# reads as "matched nothing" even though a real token was consumed, and
# `do_scanf` returns early right there. Any `sscanf`/`fscanf` format with a
# `%*s` gets silently truncated at the first one — discovered via BusyBox
# `ps`/`top` (`libbb/procps.c`'s `/proc/<pid>/stat` parser skips half its
# fields with exactly that conversion) reading every field correctly but
# still coming back empty. Upstream bug, not specific to this port; patched
# here (not forked) so it keeps applying across submodule updates.
if ! grep -q "must advance even when there's nowhere to store" \
        mlibc/options/ansi/generic/stdio.cpp; then
    python3 - "$REPO_ROOT/mlibc/options/ansi/generic/stdio.cpp" <<'PYEOF'
import sys

path = sys.argv[1]
with open(path) as f:
    content = f.read()

old = '''		const auto append_to_buffer = [&](char c) {
			if(allocate_buf) {
				temp_dest += c;
				count++;
			} else {
				char *typed_dest = (char *)dest;
				if(typed_dest)
					typed_dest[count++] = c;
			}
		};'''

new = '''		const auto append_to_buffer = [&](char c) {
			// `count` must advance even when there's nowhere to store `c`
			// (a suppressed `%*s`/`%*c`/`%*[` conversion: `dest` is null,
			// `allocate_buf` is false) — every caller of this lambda
			// (the 's'/'c'/'[' cases below) uses `count == 0` afterwards
			// to decide whether the conversion matched anything at all
			// (NOMATCH_CHECK(count == 0)). With the increment inside the
			// `if(typed_dest)` branch, every suppressed conversion looked
			// like "matched nothing" regardless of what was actually
			// consumed, so `do_scanf` returned early right at the first
			// `%*s` in a format string.
			if(allocate_buf) {
				temp_dest += c;
			} else {
				char *typed_dest = (char *)dest;
				if(typed_dest)
					typed_dest[count] = c;
			}
			count++;
		};'''

if old not in content:
    print("error: mlibc's do_scanf append_to_buffer lambda doesn't match the "
          "expected text (upstream mlibc changed) -- setup-mlibc.sh's scanf "
          "patch needs updating", file=sys.stderr)
    sys.exit(1)

content = content.replace(old, new, 1)
with open(path, "w") as f:
    f.write(content)
PYEOF
    echo "setup-mlibc: patched do_scanf's suppressed-conversion count bug"
fi

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
