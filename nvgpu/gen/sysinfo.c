#include <stdint.h>
#include <stddef.h>
typedef uint8_t u8; typedef uint16_t u16; typedef uint32_t u32; typedef uint64_t u64; typedef int32_t s32; typedef int64_t s64; typedef int8_t s8; typedef int16_t s16;
#define __packed __attribute__((packed))
#include <stdio.h>
#include <string.h>
#include "nvkm/subdev/gsp/rm/r570/nvrm/gsp.h"
#define O(f) printf("  %s = %zu\n", #f, offsetof(GspSystemInfo,f))
int main(void){
 printf("GspSystemInfo size %zu\n", sizeof(GspSystemInfo));
 O(gpuPhysAddr);O(gpuPhysFbAddr);O(gpuPhysInstAddr);O(gpuPhysIoAddr);O(nvDomainBusDeviceFunc);O(simAccessBufPhysAddr);O(notifyOpSharedSurfacePhysAddr);O(pcieAtomicsOpMask);O(consoleMemSize);O(maxUserVa);O(pciConfigMirrorBase);O(pciConfigMirrorSize);O(PCIDeviceID);O(PCISubDeviceID);O(PCIRevisionID);O(pcieAtomicsCplDeviceCapMask);O(oorArch);O(clPdbProperties);O(Chipset);O(bGpuBehindBridge);O(bFlrSupported);O(b64bBar0Supported);O(bMnocAvailable);O(chipsetL1ssEnable);O(bUpstreamL0sUnsupported);O(bSystemHasMux);O(upstreamAddressValid);O(FHBBusInfo);O(chipsetIDInfo);O(acpiMethodData);O(hypervisorType);O(bIsPassthru);O(sysTimerOffsetNs);O(gspVFInfo);O(bIsPrimary);O(isGridBuild);O(pcieConfigReg);O(gridBuildCsp);O(bPreserveVideoMemoryAllocations);O(bTdrEventSupported);O(bFeatureStretchVblankCapable);O(bEnableDynamicGranularityPageArrays);O(bClockBoostSupported);O(bRouteDispIntrsToCPU);O(hostPageSize);
 return 0;}
