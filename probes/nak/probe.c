/* Compiles one compute shader with NAK for the GA106 (SM86) and prints what came out. Built twice: native glibc (to debug the
 * probe) and static musl (the binary that runs on constanos). The output is deterministic, so the two must print the same hash. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include "nak.h"
#include "nir_builder.h"
#include "nv_device_info.h"

/* The driver (nvk_shader.c) defines where NAK finds sample info and the printf buffer; this shader uses neither. */
const struct nak_constant_offset_info nak_const_offsets_base = {0};
const struct nak_constant_offset_info nak_const_offsets_turing_graphics = {0};

static uint64_t fnv1a(const uint8_t *p, size_t n) {
   uint64_t h = 0xcbf29ce484222325ull;
   for (size_t i = 0; i < n; i++) { h ^= p[i]; h *= 0x100000001b3ull; }
   return h;
}

int main(void) {
   struct nv_device_info info;
   memset(&info, 0, sizeof info);
   info.type = NV_DEVICE_TYPE_DIS;
   info.device_id = 0x2504;           /* RTX 3050 (GA106) */
   info.chipset = 0x196;
   strcpy(info.device_name, "NVIDIA GeForce RTX 3050");
   strcpy(info.chipset_name, "GA106");
   info.sm = 86;
   info.gpc_count = 3; info.tpc_count = 10; info.mp_per_tpc = 2;
   info.max_warps_per_mp = 48; info.max_blocks_per_mp = 16;
   info.cls_compute = 0xc7c0;
   info.max_smem_per_wg_kB = 99;
   info.sm_smem_sizes_kB[0] = 100; info.sm_smem_size_count = 1;

   struct nak_compiler *nak = nak_compiler_create(&info);
   if (!nak) { puts("nak_compiler_create failed"); return 1; }

   nir_builder b = nir_builder_init_simple_shader(MESA_SHADER_COMPUTE, nak_nir_options(nak), "probe");
   b.shader->info.workgroup_size[0] = 64;
   b.shader->info.workgroup_size[1] = 1;
   b.shader->info.workgroup_size[2] = 1;
   nir_def *tid = nir_channel(&b, nir_load_local_invocation_id(&b), 0);
   nir_def *val = nir_iadd_imm(&b, nir_imul_imm(&b, tid, 3), 1234);
   nir_def *addr = nir_iadd(&b, nir_imm_int64(&b, 0x1000), nir_u2u64(&b, nir_imul_imm(&b, tid, 4)));
   nir_store_global(&b, val, addr, .write_mask = 0x1, .align_mul = 4);

   nir_shader *nir = b.shader;
   nak_preprocess_nir(nir, nak);
   nir->info.io_lowered = true;   /* a compute shader with no varyings: nothing to lower */
   struct nak_fs_key fs_key; memset(&fs_key, 0, sizeof fs_key);
   nak_postprocess_nir(nir, nak, 0, &fs_key, false);
   struct nak_shader_bin *bin = nak_compile_shader(nir, true, nak, 0, &fs_key, false);
   if (!bin) { puts("nak_compile_shader failed"); return 2; }

   printf("NAK sm=%u gprs=%u instrs=%u code_size=%u hash=%016llx\n", bin->info.sm, bin->info.num_gprs, bin->info.num_instrs,
          bin->code_size, (unsigned long long)fnv1a(bin->code, bin->code_size));
   if (bin->asm_str) printf("%s\n", bin->asm_str);
   puts("NAK PROBE DONE");
   return 0;
}
