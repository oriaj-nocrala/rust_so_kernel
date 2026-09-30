// Oracle for nvgpu::qmd (phase 7b): the words of a QMD V03_00 built field by field from NVIDIA's own header
// (open-gpu-doc/classes/compute/clc6c0qmd.h, the version Mesa's NAK picks for AMPERE_COMPUTE_A and later).
// The `MW(hi:lo)` ranges of the header are cut with the `(1?x)` / `(0?x)` trick of nvmisc.h.
//
//   D=~/src/gpu-ref/open-gpu-doc/classes/compute
//   clang -I $D -w -o /tmp/qmd nvgpu/gen/qmd.c
//   /tmp/qmd words  > nvgpu/fixtures/qmd-fill.txt     # the 64 words of the dispatch below
//   /tmp/qmd fields > nvgpu/fixtures/qmd-fields.txt   # `NAME hi lo` of every field `nvgpu::qmd` sets (the header's MW ranges)
//
// The dispatch is the one `gpu=compute` runs: `shader-fill-sm86.bin` (8 registers) over 8 CTAs of 32 threads, cbuf 0 of
// 0x200 bytes, no shared memory (SM86: sizes 0/8/16/32/64/100 KiB -> MIN 1, TARGET 1, MAX 26), and the grid's own release
// of a one-word semaphore.
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#define MW(x) x
#include "clc6c0qmd.h"

#define HI(f) (1 ? f)
#define LO(f) (0 ? f)
#define F(name) NVC6C0_QMDV03_00_##name

static uint32_t q[64];

static void put(int hi, int lo, uint64_t v) {
    for (int b = lo; b <= hi; b++) {
        if ((v >> (b - lo)) & 1) q[b / 32] |= 1u << (b % 32);
        else q[b / 32] &= ~(1u << (b % 32));
    }
}
#define SET(name, v) put(HI(F(name)), LO(F(name)), (v))
#define SETI(name, i, v) put(HI(F(name)(i)), LO(F(name)(i)), (v))

static void fields(void) {
#define FIELD(name) printf("%s %d %d\n", #name, HI(F(name)), LO(F(name)))
    FIELD(QMD_VERSION); FIELD(QMD_MAJOR_VERSION); FIELD(API_VISIBLE_CALL_LIMIT); FIELD(SAMPLER_INDEX);
    FIELD(SM_GLOBAL_CACHING_ENABLE); FIELD(BARRIER_COUNT);
    FIELD(CTA_RASTER_WIDTH); FIELD(CTA_RASTER_HEIGHT); FIELD(CTA_RASTER_DEPTH);
    FIELD(CTA_THREAD_DIMENSION0); FIELD(CTA_THREAD_DIMENSION1); FIELD(CTA_THREAD_DIMENSION2);
    FIELD(PROGRAM_ADDRESS_LOWER); FIELD(PROGRAM_ADDRESS_UPPER); FIELD(REGISTER_COUNT_V);
    FIELD(SHADER_LOCAL_MEMORY_LOW_SIZE); FIELD(SHADER_LOCAL_MEMORY_HIGH_SIZE);
    FIELD(SHARED_MEMORY_SIZE); FIELD(MIN_SM_CONFIG_SHARED_MEM_SIZE); FIELD(MAX_SM_CONFIG_SHARED_MEM_SIZE);
    FIELD(TARGET_SM_CONFIG_SHARED_MEM_SIZE);
    FIELD(RELEASE0_ADDRESS_LOWER); FIELD(RELEASE0_ADDRESS_UPPER); FIELD(RELEASE0_MEMBAR_TYPE); FIELD(RELEASE0_ENABLE);
    FIELD(RELEASE0_STRUCTURE_SIZE); FIELD(RELEASE0_PAYLOAD_LOWER);
    for (int i = 0; i < 8; i++) {
        printf("CONSTANT_BUFFER_VALID(%d) %d %d\n", i, HI(F(CONSTANT_BUFFER_VALID)(i)), LO(F(CONSTANT_BUFFER_VALID)(i)));
        printf("CONSTANT_BUFFER_ADDR_LOWER(%d) %d %d\n", i, HI(F(CONSTANT_BUFFER_ADDR_LOWER)(i)), LO(F(CONSTANT_BUFFER_ADDR_LOWER)(i)));
        printf("CONSTANT_BUFFER_ADDR_UPPER(%d) %d %d\n", i, HI(F(CONSTANT_BUFFER_ADDR_UPPER)(i)), LO(F(CONSTANT_BUFFER_ADDR_UPPER)(i)));
        printf("CONSTANT_BUFFER_SIZE_SHIFTED4(%d) %d %d\n", i, HI(F(CONSTANT_BUFFER_SIZE_SHIFTED4)(i)), LO(F(CONSTANT_BUFFER_SIZE_SHIFTED4)(i)));
    }
    // the values the header names for the enums `nvgpu::qmd` sets
    printf("API_VISIBLE_CALL_LIMIT_NO_CHECK %d\n", F(API_VISIBLE_CALL_LIMIT_NO_CHECK));
    printf("SAMPLER_INDEX_INDEPENDENTLY %d\n", F(SAMPLER_INDEX_INDEPENDENTLY));
    printf("RELEASE0_MEMBAR_TYPE_FE_SYSMEMBAR %d\n", F(RELEASE0_MEMBAR_TYPE_FE_SYSMEMBAR));
    printf("RELEASE0_STRUCTURE_SIZE_SEMAPHORE_ONE_WORD %d\n", F(RELEASE0_STRUCTURE_SIZE_SEMAPHORE_ONE_WORD));
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "fields")) {
        fields();
        return 0;
    }
    const uint64_t kern = 0x380040000ull;
    const uint64_t prog = kern + 0x000, cb0 = kern + 0x800, rel = kern + 0x2010;
    SET(QMD_MAJOR_VERSION, 3);
    SET(QMD_VERSION, 0);
    SET(API_VISIBLE_CALL_LIMIT, F(API_VISIBLE_CALL_LIMIT_NO_CHECK));
    SET(SAMPLER_INDEX, F(SAMPLER_INDEX_INDEPENDENTLY));
    SET(SM_GLOBAL_CACHING_ENABLE, 1);
    SET(BARRIER_COUNT, 0);
    SET(CTA_RASTER_WIDTH, 8);
    SET(CTA_RASTER_HEIGHT, 1);
    SET(CTA_RASTER_DEPTH, 1);
    SET(CTA_THREAD_DIMENSION0, 32);
    SET(CTA_THREAD_DIMENSION1, 1);
    SET(CTA_THREAD_DIMENSION2, 1);
    SET(PROGRAM_ADDRESS_LOWER, (uint32_t)prog);
    SET(PROGRAM_ADDRESS_UPPER, prog >> 32);
    SET(REGISTER_COUNT_V, 8);
    SET(SHADER_LOCAL_MEMORY_HIGH_SIZE, 0);
    SET(SHADER_LOCAL_MEMORY_LOW_SIZE, 0);
    SET(DEPENDENCE_COUNTER, 0);
    SET(HW_ONLY_DEPENDENCE_COUNTER, 0);
    SET(SHARED_MEMORY_SIZE, 0);
    SET(MIN_SM_CONFIG_SHARED_MEM_SIZE, 1);
    SET(MAX_SM_CONFIG_SHARED_MEM_SIZE, 26);
    SET(TARGET_SM_CONFIG_SHARED_MEM_SIZE, 1);
    SETI(CONSTANT_BUFFER_ADDR_LOWER, 0, (uint32_t)cb0);
    SETI(CONSTANT_BUFFER_ADDR_UPPER, 0, cb0 >> 32);
    SETI(CONSTANT_BUFFER_SIZE_SHIFTED4, 0, 0x200 >> 4);
    SETI(CONSTANT_BUFFER_VALID, 0, 1);
    SET(RELEASE0_ADDRESS_LOWER, (uint32_t)rel);
    SET(RELEASE0_ADDRESS_UPPER, rel >> 32);
    SET(RELEASE0_MEMBAR_TYPE, F(RELEASE0_MEMBAR_TYPE_FE_SYSMEMBAR));
    SET(RELEASE0_ENABLE, 1);
    SET(RELEASE0_STRUCTURE_SIZE, F(RELEASE0_STRUCTURE_SIZE_SEMAPHORE_ONE_WORD));
    SET(RELEASE0_PAYLOAD_LOWER, 0x4242);
    for (int i = 0; i < 64; i++) printf("%d %08x\n", i, q[i]);
    return 0;
}
