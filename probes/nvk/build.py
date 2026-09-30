#!/usr/bin/env python3
"""build.py : compile vk_probe.c and link it with the whole of NVK (built by Meson as probes/nak/README.md and mesa-port/README.md say)
into one static musl executable, ~/src/gpu-ref/nvk-probe/vk-probe. Needs the stubs in stubs.c for the few third-party symbols NVK
references that a constanos process never reaches (DRM, udev, libelf, SPIRV-Tools, libdisplay-info)."""
import os, subprocess, sys
home = os.path.expanduser('~/src/gpu-ref')
bdir = os.path.join(home, 'mesa/build-musl')
here = os.path.dirname(os.path.abspath(__file__))
out_dir = os.path.join(home, 'nvk-probe')
os.makedirs(out_dir, exist_ok=True)
cc = ['clang', '--target=x86_64-linux-musl', '-nostdlibinc', '-isystem', '/usr/lib/musl/include', '-isystem', home + '/musl-inc',
      '-O2', '-g', '-D_GNU_SOURCE', '-fno-strict-aliasing', '-isystem', '/usr/lib/gcc/x86_64-pc-linux-gnu/16/include']
objs = []
for src in ['vk_probe.c', 'stubs.c']:
    p = os.path.join(here, src)
    if not os.path.exists(p):
        continue
    o = os.path.join(out_dir, src.replace('.c', '.o'))
    r = subprocess.run(cc + ['-I', here, '-c', p, '-o', o])
    if r.returncode:
        sys.exit(r.returncode)
    objs.append(o)

libs = """src/nouveau/vulkan/libnvk_rs.a src/nouveau/vulkan/libnvk_bindings_rs_extern.a
src/nouveau/compiler/libnak.a src/nouveau/compiler/libnak_rs.a src/compiler/rust/libcompiler_bindings.a src/util/libmesa_util.a
src/util/libmesa_util_clflush.a src/util/libmesa_util_clflushopt.a src/util/libmesa_util_simd.a src/util/blake3/libblake3.a
src/c11/impl/libmesa_util_c11.a src/compiler/rust/libcompiler_c_helpers.a src/nouveau/headers/libnvidia_headers_c.a
src/nouveau/nil/libnil.a src/nouveau/nil/liblibnil_format_table.a src/compiler/nir/libnir.a src/compiler/libcompiler.a
src/nouveau/cubin/libnouveau_cubin.a src/nouveau/mme/libnouveau_mme.a src/vulkan/util/libvulkan_util.a src/compiler/spirv/libvtn.a
src/util/libxmlconfig.a""".split()
libs = [os.path.join(bdir, l) for l in libs]
m = '/usr/lib/musl/lib/'
gcc = '/usr/lib/gcc/x86_64-pc-linux-gnu/16/'
rustlib = os.path.expanduser('~/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/lib/rustlib/x86_64-unknown-linux-musl/lib/self-contained/')
out = os.path.join(out_dir, 'vk-probe')
link = ['clang++', '--target=x86_64-linux-musl', '-static', '-nostdlib', '-fuse_ld=lld' if False else '-fuse-ld=lld', '-o', out,
        m + 'crt1.o', m + 'crti.o'] + objs + ['-Wl,--whole-archive', os.path.join(bdir, 'src/nouveau/vulkan/libnvk.a'), '-Wl,--no-whole-archive', '-Wl,--start-group'] + libs + ['-Wl,--end-group', '-Wl,--gc-sections', '-Wl,--build-id=sha1', '-Wl,--eh-frame-hdr',
        gcc + 'libstdc++.a', m + 'libm.a', m + 'libc.a', gcc + 'libgcc.a', rustlib + 'libunwind.a', m + 'libc.a', m + 'crtn.o']
r = subprocess.run(link)
sys.exit(r.returncode)
