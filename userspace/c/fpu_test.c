// Regression test for FPU/SSE state save/restore across context switches
// (kernel/src/process/fpu.rs). Loads a distinctive 128-bit pattern into
// xmm0 via inline asm, spins in a long pure-integer loop (long enough to
// span several timer-preemption time slices — PIT runs at 100Hz, and the
// scheduler's quantum is a handful of ticks, so tens of milliseconds), then
// reads xmm0 back out. If any context switch during the spin failed to
// save/restore FPU state, xmm0 comes back corrupted (either zeroed, or
// holding some other process's register contents).
#include <stdio.h>
#include <stdint.h>

#define SPIN_ITERS 300000000ULL
#define ROUNDS 10

int main(void) {
    for (int round = 0; round < ROUNDS; round++) {
        uint64_t pattern_lo = 0x1122334455667788ULL ^ ((uint64_t)round * 0x0101010101010101ULL);
        uint64_t pattern_hi = 0x99AABBCCDDEEFF00ULL ^ ((uint64_t)round * 0x1010101010101010ULL);
        uint64_t check_lo, check_hi;
        uint64_t iters = SPIN_ITERS;

        __asm__ volatile(
            "movq %[lo], %%xmm0\n\t"
            "movq %[hi], %%xmm1\n\t"
            "punpcklqdq %%xmm1, %%xmm0\n\t"
            "1:\n\t"
            "dec %[cnt]\n\t"
            "jnz 1b\n\t"
            "movq %%xmm0, %[out_lo]\n\t"
            "pshufd $0xEE, %%xmm0, %%xmm1\n\t" // move high 64 bits into low 64 of xmm1
            "movq %%xmm1, %[out_hi]\n\t"
            : [out_lo] "=r"(check_lo), [out_hi] "=r"(check_hi), [cnt] "+r"(iters)
            : [lo] "r"(pattern_lo), [hi] "r"(pattern_hi)
            : "xmm0", "xmm1", "cc"
        );

        if (check_lo != pattern_lo || check_hi != pattern_hi) {
            printf("fpu_test: FAILED round %d — xmm0 corrupted across spin\n", round);
            printf("  expected lo=%llx hi=%llx\n", (unsigned long long)pattern_lo, (unsigned long long)pattern_hi);
            printf("  got      lo=%llx hi=%llx\n", (unsigned long long)check_lo, (unsigned long long)check_hi);
            return 1;
        }
        printf("fpu_test: round %d OK (lo=%llx hi=%llx)\n", round, (unsigned long long)check_lo, (unsigned long long)check_hi);
    }

    printf("fpu_test: ALL_OK\n");
    return 0;
}
