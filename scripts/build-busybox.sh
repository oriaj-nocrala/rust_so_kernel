#!/usr/bin/env bash
# scripts/build-busybox.sh
#
# Cross-compiles BusyBox (git submodule, pinned at 1_36_1) against
# sysroot/ and drops the resulting static ELF at kernel/embedded/busybox.elf.
#
# Requires sysroot/ to already exist — run scripts/setup-mlibc.sh (or just
# `cargo build`, which does it automatically) first.
#
# busybox-config/minimal.config is the tracked starting point (currently:
# CONFIG_STATIC + CONFIG_TRUE + CONFIG_ECHO only — a deliberately tiny
# smoke-test set, see the busybox-readiness memory/session notes for how
# it was arrived at and what broke getting here). `make oldconfig` fills
# in anything new the pinned BusyBox version added since with defaults, so
# this keeps working across submodule bumps without hand-editing again.
#
# Idempotent: safe to re-run; only rebuilds what changed.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

if [ ! -f sysroot/usr/lib/libc.a ]; then
    echo "build-busybox: sysroot/ missing — run scripts/setup-mlibc.sh first" >&2
    exit 1
fi

if [ ! -f busybox/Makefile ]; then
    echo "build-busybox: busybox/ submodule not checked out yet — initializing..."
    git submodule update --init busybox
fi

for tool in clang llvm-ar llvm-strip make; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "error: required build tool '$tool' not found in PATH." >&2
        exit 1
    fi
done

# ── 1. Cross-compiler wrapper ───────────────────────────────────────────────
#
# BusyBox's own Makefile invokes the same $(CC) both to compile (-c) and,
# via `LD = $(CC) -nostdlib`, to do per-directory partial/relocatable
# links (-r) as well as the final executable link. A "normal" cross
# toolchain has crt startup + libc wired in transparently (via a
# --sysroot with proper specs); ours doesn't, so this wrapper injects
# crt1.o/libc.a/-static itself — but ONLY for the real final link, never
# for a `-r` partial link. Getting that distinction wrong is exactly what
# broke this the first time: crt1.o's `_start` ended up spliced into every
# intermediate built-in.o, then collided with the real crt1.o at the
# actual final link ("multiple definition of `_start`").
mkdir -p build-busybox
CC_WRAPPER="$REPO_ROOT/build-busybox/cc-wrapper.sh"
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
    # No separate libm.a/librt.a exist — musl's math functions are already
    # bundled straight into libc.a — so drop -lm/-lrt rather than fail
    # with "cannot find -lm".
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

cp busybox-config/minimal.config busybox/.config
# `yes` exits via SIGPIPE once `make oldconfig` stops reading (as soon as
# it's seen every new question) — normal for this idiom, but `pipefail`
# would otherwise turn that harmless SIGPIPE into a script-ending failure.
(cd busybox && set +o pipefail; yes "" | make oldconfig >/dev/null)

# ── 3. Build ──────────────────────────────────────────────────────────────

make -C busybox \
    CC="$CC_WRAPPER" \
    AR=llvm-ar \
    STRIP=llvm-strip \
    HOSTCC=cc \
    -j"$(nproc)"

# ── 4. Embed ──────────────────────────────────────────────────────────────

mkdir -p kernel/embedded
cp busybox/busybox kernel/embedded/busybox.elf

echo "build-busybox: kernel/embedded/busybox.elf ready"
