#!/bin/bash
# Build NVK for constanos and the probe programs (docs/gpu/g4-nvkmd-plan.md):
#   1. apply the constanos changes to the Mesa checkout (apply.sh)
#   2. native tools Mesa needs at build time (mesa_clc, vtn_bindgen2) from a native build
#   3. a cross build for x86_64-linux-musl, static, with NVK set to talk to /dev/nvgpu (-Dnvk-constanos=true)
#   4. the probes: probes/nak (NAK alone) and probes/nvk (all of NVK, one static executable: ~/src/gpu-ref/nvk-probe/vk-probe)
# Needs: meson ninja clang lld llvm spirv-llvm-translator musl python-mako python-yaml, cargo bindgen-cli and cbindgen; the Mesa checkout
# with `git sparse-checkout add src subprojects bin`; header links in ~/src/gpu-ref/musl-inc (probes/nak/README.md).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
mesa="${MESA_DIR:-$HOME/src/gpu-ref/mesa}"
tools="$HOME/src/gpu-ref/native-tools"
"$here/apply.sh" "$mesa"
cd "$mesa"
if [ ! -x "$tools/mesa_clc" ]; then
    [ -d build-nak ] || meson setup build-nak -Dvulkan-drivers=nouveau -Dgallium-drivers= -Dplatforms= -Dllvm=enabled -Dshared-llvm=enabled \
        -Dopengl=false -Dgles1=disabled -Dgles2=disabled -Degl=disabled -Dgbm=disabled -Dglx=disabled -Dvideo-codecs= -Dvulkan-layers= \
        -Dbuild-tests=false -Dtools= -Dlibunwind=disabled -Dlmsensors=disabled -Dxlib-lease=disabled -Dwerror=false
    ninja -C build-nak src/compiler/clc/mesa_clc src/compiler/spirv/vtn_bindgen2
    mkdir -p "$tools"
    cp build-nak/src/compiler/clc/mesa_clc build-nak/src/compiler/spirv/vtn_bindgen2 "$tools/"
fi
export PATH="$tools:$PATH"
if [ ! -d build-musl ]; then
    meson setup build-musl --cross-file "$here/musl-cross.ini" --default-library=static -Dvulkan-drivers=nouveau -Dgallium-drivers= \
        -Dplatforms= -Dllvm=disabled -Dopengl=false -Dgles1=disabled -Dgles2=disabled -Degl=disabled -Dgbm=disabled -Dglx=disabled \
        -Dvideo-codecs= -Dvulkan-layers= -Dbuild-tests=false -Dtools= -Dlibunwind=disabled -Dlmsensors=disabled -Dxlib-lease=disabled \
        -Dwerror=false -Dzlib=disabled -Dzstd=disabled -Dexpat=disabled -Dshader-cache=disabled -Dmesa-clc=system \
        -Dprecomp-compiler=system -Dnvk-constanos=true
fi
# Everything compiles; only the shared-library link at the end fails (it wants glibc's libdrm), and is not needed.
ninja -C build-musl -k 0 >/dev/null 2>&1 || true
ninja -C build-musl src/nouveau/vulkan/libnvk.a src/nouveau/compiler/libnak_rs.a
python3 "$here/../probes/nak/build.py" musl
python3 "$here/../probes/nvk/build.py"
echo "built: ~/src/gpu-ref/nak-probe/nak-probe-musl and ~/src/gpu-ref/nvk-probe/vk-probe"
