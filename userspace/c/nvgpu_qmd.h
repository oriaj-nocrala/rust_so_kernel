// A QMD V03_00 (the compute launch descriptor of Ampere, NVIDIA's clc6c0qmd.h) for the launches of nvgpu_hw_test: one grid, constant
// buffer 0, no shared or local memory, and the grid's own release of a one-word semaphore. The bit ranges are the header's `MW(hi:lo)`;
// nvgpu's host test `qmd::tests::the_c_header_builds_the_oracles_words` compiles this file with clang and compares it with the words
// `nvgpu/gen/qmd.c` builds from the real header, so a wrong range shows there.
#pragma once
#include <stdint.h>

struct nvg_qmd_launch {
   uint64_t program;         // GPU VA of the SASS, 0x80-aligned
   uint32_t registers;
   uint32_t grid[3];
   uint32_t block[3];
   uint64_t cbuf0;           // GPU VA and size of constant buffer 0
   uint32_t cbuf0_size;
   uint64_t release;         // GPU VA of the semaphore the grid releases (0: none)
   uint32_t release_payload;
};

static inline void nvg_qmd_put(uint32_t *q, int hi, int lo, uint64_t v) {
   for (int b = lo; b <= hi; b++) {
      if ((v >> (b - lo)) & 1) q[b / 32] |= 1u << (b % 32);
      else q[b / 32] &= ~(1u << (b % 32));
   }
}

static inline void nvg_qmd_build(uint32_t q[64], const struct nvg_qmd_launch *l) {
   for (int i = 0; i < 64; i++) q[i] = 0;
   nvg_qmd_put(q, 583, 580, 3);              // QMD_MAJOR_VERSION
   nvg_qmd_put(q, 579, 576, 0);              // QMD_VERSION
   nvg_qmd_put(q, 378, 378, 1);              // API_VISIBLE_CALL_LIMIT = NO_CHECK
   nvg_qmd_put(q, 382, 382, 0);              // SAMPLER_INDEX = INDEPENDENTLY
   nvg_qmd_put(q, 134, 134, 1);              // SM_GLOBAL_CACHING_ENABLE
   nvg_qmd_put(q, 767, 763, 0);              // BARRIER_COUNT
   nvg_qmd_put(q, 415, 384, l->grid[0]);     // CTA_RASTER_WIDTH
   nvg_qmd_put(q, 431, 416, l->grid[1]);     // CTA_RASTER_HEIGHT
   nvg_qmd_put(q, 463, 448, l->grid[2]);     // CTA_RASTER_DEPTH
   nvg_qmd_put(q, 607, 592, l->block[0]);    // CTA_THREAD_DIMENSION0..2
   nvg_qmd_put(q, 623, 608, l->block[1]);
   nvg_qmd_put(q, 639, 624, l->block[2]);
   nvg_qmd_put(q, 1567, 1536, (uint32_t)l->program);   // PROGRAM_ADDRESS_LOWER
   nvg_qmd_put(q, 1584, 1568, l->program >> 32);       // PROGRAM_ADDRESS_UPPER
   nvg_qmd_put(q, 656, 648, l->registers);   // REGISTER_COUNT_V
   nvg_qmd_put(q, 1623, 1600, 0);            // SHADER_LOCAL_MEMORY_HIGH_SIZE
   nvg_qmd_put(q, 759, 736, 0);              // SHADER_LOCAL_MEMORY_LOW_SIZE
   nvg_qmd_put(q, 561, 544, 0);              // SHARED_MEMORY_SIZE
   nvg_qmd_put(q, 567, 562, 1);              // MIN_SM_CONFIG_SHARED_MEM_SIZE (no shared memory: the 8 KiB configuration is hw 1)
   nvg_qmd_put(q, 574, 569, 26);             // MAX_SM_CONFIG_SHARED_MEM_SIZE (100 KiB / 4 + 1)
   nvg_qmd_put(q, 662, 657, 1);              // TARGET_SM_CONFIG_SHARED_MEM_SIZE
   nvg_qmd_put(q, 1055, 1024, (uint32_t)l->cbuf0);     // CONSTANT_BUFFER_ADDR_LOWER(0)
   nvg_qmd_put(q, 1072, 1056, l->cbuf0 >> 32);         // CONSTANT_BUFFER_ADDR_UPPER(0)
   nvg_qmd_put(q, 1087, 1075, l->cbuf0_size >> 4);     // CONSTANT_BUFFER_SIZE_SHIFTED4(0)
   nvg_qmd_put(q, 640, 640, 1);              // CONSTANT_BUFFER_VALID(0)
   if (l->release) {
      nvg_qmd_put(q, 799, 768, (uint32_t)l->release);  // RELEASE0_ADDRESS_LOWER
      nvg_qmd_put(q, 807, 800, l->release >> 32);      // RELEASE0_ADDRESS_UPPER
      nvg_qmd_put(q, 819, 819, 1);                     // RELEASE0_MEMBAR_TYPE = FE_SYSMEMBAR
      nvg_qmd_put(q, 823, 823, 1);                     // RELEASE0_ENABLE
      nvg_qmd_put(q, 831, 830, 1);                     // RELEASE0_STRUCTURE_SIZE = SEMAPHORE_ONE_WORD
      nvg_qmd_put(q, 863, 832, l->release_payload);    // RELEASE0_PAYLOAD_LOWER
   }
}
