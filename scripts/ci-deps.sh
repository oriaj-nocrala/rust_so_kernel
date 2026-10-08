#!/usr/bin/env bash
# scripts/ci-deps.sh — the Ubuntu 24.04 packages a clean build, the QEMU tests and
# scripts/make-release-image.sh need. Used by .github/workflows/ci.yml (and by hand
# in an ubuntu:24.04 container, to try a CI change before pushing it). Rust comes
# from rustup and rust-toolchain.toml, not from here.
set -euo pipefail
SUDO=""
[[ $(id -u) -eq 0 ]] || SUDO=sudo
export DEBIAN_FRONTEND=noninteractive
$SUDO apt-get update -q
$SUDO apt-get install -y -q --no-install-recommends \
    build-essential clang lld llvm meson ninja-build python3 \
    qemu-system-x86 qemu-utils ovmf \
    e2fsprogs dosfstools mtools fdisk zstd rsync socat \
    curl unzip ca-certificates git
