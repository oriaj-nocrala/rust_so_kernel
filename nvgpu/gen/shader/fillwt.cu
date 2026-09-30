// fill.cu with a write-through store (`st.global.wt`) and other values: a store policy that should not stay dirty in the L2.
extern "C" __global__ void fillwt(unsigned *out) {
    unsigned i = blockIdx.x * 32u + threadIdx.x;
    __stwt(out + i, ((i * 0x9e3779b1u) ^ 0xc0de0000u) ^ 0x12345678u);
}
