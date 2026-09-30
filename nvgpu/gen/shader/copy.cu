// dst[i] = src[i] through L2 only (`ld.global.cg`): the GPU's own view of memory, to tell "the stores did not land" from "the
// CPU cannot see them yet".
extern "C" __global__ void copyw(unsigned *dst, const unsigned *src) {
    unsigned i = blockIdx.x * 32u + threadIdx.x;
    dst[i] = __ldcg(src + i);
}
