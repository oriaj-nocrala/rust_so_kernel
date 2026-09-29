#include <stdint.h>
#include <stddef.h>
typedef uint8_t u8; typedef uint16_t u16; typedef uint32_t u32; typedef uint64_t u64; typedef int32_t s32; typedef int64_t s64; typedef int8_t s8; typedef int16_t s16;
#define __packed __attribute__((packed))
#include <stdio.h>
#include <string.h>
#include "nvkm/subdev/gsp/rm/r570/nvrm/gsp.h"
#include "nvkm/subdev/gsp/rm/r535/nvrm/gsp.h"
#define O(f) printf("  %s = %zu\n", #f, offsetof(GspStaticConfigInfo,f))
int main(void){
 printf("GspStaticConfigInfo size %zu\n", sizeof(GspStaticConfigInfo));
 O(gpuNameString); O(gpuNameString_Unicode); O(bar1PdeBase); O(bar2PdeBase); O(hInternalClient); O(hInternalDevice); O(hInternalSubdevice);
 return 0;}
