// Oracle for the PERF controls' parameter layouts (OpenRM 570.144 ctrl2080perf.h).
// clang -w -I $R/src/common/sdk/nvidia/inc -I $R/src/common/sdk/nvidia/inc/ctrl perf.c -o /tmp/perf && /tmp/perf   (R = ~/src/gpu-ref/open-gpu-kernel-modules)
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include "ctrl/ctrl2080/ctrl2080perf.h"
#define O(t,f) printf("  %s.%s = %zu\n", #t, #f, offsetof(t,f))
int main(void){
 printf("GET_CURRENT_PSTATE size %zu\n", sizeof(NV2080_CTRL_PERF_GET_CURRENT_PSTATE_PARAMS));
 printf("GET_CLK_INFO size %zu\n", sizeof(NV2080_CTRL_PERF_GET_CLK_INFO));
 O(NV2080_CTRL_PERF_GET_CLK_INFO,flags);O(NV2080_CTRL_PERF_GET_CLK_INFO,domain);O(NV2080_CTRL_PERF_GET_CLK_INFO,currentFreq);O(NV2080_CTRL_PERF_GET_CLK_INFO,defaultFreq);O(NV2080_CTRL_PERF_GET_CLK_INFO,minFreq);O(NV2080_CTRL_PERF_GET_CLK_INFO,maxFreq);
 printf("GET_LEVEL_INFO_V2 size %zu\n", sizeof(NV2080_CTRL_PERF_GET_LEVEL_INFO_V2_PARAMS));
 O(NV2080_CTRL_PERF_GET_LEVEL_INFO_V2_PARAMS,level);O(NV2080_CTRL_PERF_GET_LEVEL_INFO_V2_PARAMS,flags);O(NV2080_CTRL_PERF_GET_LEVEL_INFO_V2_PARAMS,perfGetClkInfoList);O(NV2080_CTRL_PERF_GET_LEVEL_INFO_V2_PARAMS,perfGetClkInfoListSize);
 return 0;}
