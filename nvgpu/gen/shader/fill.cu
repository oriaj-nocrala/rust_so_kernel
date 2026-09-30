extern "C" __global__ void fill(unsigned *out) {
    unsigned i = blockIdx.x * 32u + threadIdx.x;
    out[i] = (i * 0x9e3779b1u) ^ 0xc0de0000u;
}
