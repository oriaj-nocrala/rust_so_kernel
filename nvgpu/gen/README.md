# nvgpu/gen: ABI oracle (plan D2)

`abi.c` prints `sizeof`/`offsetof` of the GSP structures from the *real* 570.144
headers of `open-gpu-kernel-modules` (`scripts/gpu-ref.sh` clones them to `~/src/gpu-ref/`),
with a minimal `nvtypes.h`. The offsets asserted in `src/gspmem.rs` tests are its output:

    R=~/src/gpu-ref/open-gpu-kernel-modules/src
    clang -I nvgpu/gen -I $R -w -o /tmp/abi nvgpu/gen/abi.c && /tmp/abi
