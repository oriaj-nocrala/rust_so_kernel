#!/usr/bin/env python3
"""build.py native|musl : compile and link probe.c against the Mesa build dir of that flavour (build-nak / build-musl)."""
import json, re, shlex, subprocess, sys, os
flavour = sys.argv[1]
root = os.path.expanduser('~/src/gpu-ref/mesa')
bdir = os.path.join(root, 'build-nak' if flavour == 'native' else 'build-musl')
cc = json.load(open(os.path.join(bdir, 'compile_commands.json')))
cmd = [e for e in cc if e['file'].endswith('nak_nir.c')][0]['command']
cmd = re.sub(r' -MD -MQ \S+ -MF \S+', '', cmd)
cmd = re.sub(r' -o \S+ -c \S+$', '', cmd)
args = shlex.split(cmd)
if args[0].endswith('ccache'):
    args = args[1:]
src = os.path.expanduser('~/src/gpu-ref/nak-probe/probe.c')
obj = f'/tmp/probe-{flavour}.o'
r = subprocess.run(args + ['-o', obj, '-c', src], cwd=bdir)
if r.returncode: sys.exit(r.returncode)
libs = """src/nouveau/compiler/libnak.a src/nouveau/compiler/libnak_rs.a src/compiler/rust/libcompiler_bindings.a
src/util/libmesa_util.a src/util/libmesa_util_clflush.a src/util/libmesa_util_clflushopt.a src/util/libmesa_util_simd.a
src/util/blake3/libblake3.a src/c11/impl/libmesa_util_c11.a src/compiler/rust/libcompiler_c_helpers.a
src/nouveau/headers/libnvidia_headers_c.a src/nouveau/nil/libnil.a src/nouveau/nil/liblibnil_format_table.a
src/compiler/nir/libnir.a src/compiler/libcompiler.a""".split()
out = os.path.expanduser(f'~/src/gpu-ref/nak-probe/nak-probe-{flavour}')
if flavour == 'native':
    link = ['c++', obj, '-o', out, '-Wl,--start-group'] + libs + ['-Wl,--end-group', '-lm', '-lpthread', '-ldl']
else:
    m = '/usr/lib/musl/lib/'
    gcc = '/usr/lib/gcc/x86_64-pc-linux-gnu/16/'
    rustlib = os.path.expanduser('~/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/lib/rustlib/x86_64-unknown-linux-musl/lib/self-contained/')
    link = ['clang++', '--target=x86_64-linux-musl', '-static', '-nostdlib', '-fuse-ld=lld', '-o', out,
            m + 'crt1.o', m + 'crti.o', obj, '-Wl,--start-group'] + libs + ['-Wl,--end-group',
            gcc + 'libstdc++.a', m + 'libm.a', m + 'libc.a', gcc + 'libgcc.a', rustlib + 'libunwind.a', m + 'libc.a', m + 'crtn.o']
r = subprocess.run(link, cwd=bdir)
sys.exit(r.returncode)
