// Measures the SM clock from inside the GPU: SM cycles (clock64) over the nanoseconds of the global timer (%globaltimer) across a chain of dependent
// FFMAs. cycles / ns = GHz, whatever the loop does (its length only makes the interval long enough to read: 1 us resolution of the timer).
// One thread per CTA writes 8 u64 at out[ctaid.x * 8]: c0, c1, t0, t1, %smid, the result bits (so the loop is not thrown away), 0, 0.
// Parameters: out at c[0x0][0x160], the iteration count (32 bits) at c[0x0][0x168].
extern "C" __global__ void clockprobe(unsigned long long *out, unsigned iters) {
    if (threadIdx.x != 0) return;
    unsigned smid;
    asm volatile("mov.u32 %0, %%smid;" : "=r"(smid));
    unsigned long long t0, t1;
    asm volatile("mov.u64 %0, %%globaltimer;" : "=l"(t0) :: "memory");
    long long c0 = clock64();
    float x = 1.0f;
    #pragma unroll 1
    for (unsigned i = 0; i < iters; i++) x = x * 1.0000001f + 1e-7f;
    long long c1 = clock64();
    asm volatile("mov.u64 %0, %%globaltimer;" : "=l"(t1) :: "memory");
    unsigned long long *o = out + (unsigned long long)blockIdx.x * 8;
    o[0] = c0; o[1] = c1; o[2] = t0; o[3] = t1; o[4] = smid; o[5] = __float_as_uint(x); o[6] = 0; o[7] = 0;
}
