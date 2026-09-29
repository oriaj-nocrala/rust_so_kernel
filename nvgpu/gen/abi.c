#include "nvtypes.h"
#include <stdio.h>
#include "nvidia/arch/nvalloc/common/inc/gsp/gsp_fw_wpr_meta.h"
#include "common/uproc/os/common/include/libos_init_args.h"
#include "nvidia/inc/kernel/gpu/gsp/gsp_init_args.h"
#include "common/shared/msgq/inc/msgq/msgq_priv.h"
#define O(t,f) printf("  %s.%s = %zu\n", #t, #f, offsetof(t,f))
int main(void){
 printf("GspFwWprMeta size %zu\n", sizeof(GspFwWprMeta));
 O(GspFwWprMeta,magic);O(GspFwWprMeta,revision);O(GspFwWprMeta,sysmemAddrOfRadix3Elf);O(GspFwWprMeta,sizeOfRadix3Elf);O(GspFwWprMeta,sysmemAddrOfBootloader);O(GspFwWprMeta,sizeOfBootloader);O(GspFwWprMeta,bootloaderCodeOffset);O(GspFwWprMeta,bootloaderDataOffset);O(GspFwWprMeta,bootloaderManifestOffset);O(GspFwWprMeta,sysmemAddrOfSignature);O(GspFwWprMeta,sizeOfSignature);O(GspFwWprMeta,gspFwRsvdStart);O(GspFwWprMeta,nonWprHeapOffset);O(GspFwWprMeta,nonWprHeapSize);O(GspFwWprMeta,gspFwWprStart);O(GspFwWprMeta,gspFwHeapOffset);O(GspFwWprMeta,gspFwHeapSize);O(GspFwWprMeta,gspFwOffset);O(GspFwWprMeta,bootBinOffset);O(GspFwWprMeta,frtsOffset);O(GspFwWprMeta,frtsSize);O(GspFwWprMeta,gspFwWprEnd);O(GspFwWprMeta,fbSize);O(GspFwWprMeta,vgaWorkspaceOffset);O(GspFwWprMeta,vgaWorkspaceSize);O(GspFwWprMeta,bootCount);O(GspFwWprMeta,partitionRpcAddr);O(GspFwWprMeta,partitionRpcRequestOffset);O(GspFwWprMeta,partitionRpcReplyOffset);O(GspFwWprMeta,lsUcodeVersion);O(GspFwWprMeta,gspFwHeapVfPartitionCount);O(GspFwWprMeta,flags);O(GspFwWprMeta,pmuReservedSize);O(GspFwWprMeta,verified);
 printf("LibosMemoryRegionInitArgument size %zu\n", sizeof(LibosMemoryRegionInitArgument));
 O(LibosMemoryRegionInitArgument,id8);O(LibosMemoryRegionInitArgument,pa);O(LibosMemoryRegionInitArgument,size);O(LibosMemoryRegionInitArgument,kind);O(LibosMemoryRegionInitArgument,loc);
 printf("msgqTxHeader size %zu msgqRxHeader size %zu\n", sizeof(msgqTxHeader), sizeof(msgqRxHeader));
 O(msgqTxHeader,version);O(msgqTxHeader,size);O(msgqTxHeader,msgSize);O(msgqTxHeader,msgCount);O(msgqTxHeader,writePtr);O(msgqTxHeader,flags);O(msgqTxHeader,rxHdrOff);O(msgqTxHeader,entryOff);
 printf("GSP_ARGUMENTS_CACHED size %zu\n", sizeof(GSP_ARGUMENTS_CACHED));
 O(GSP_ARGUMENTS_CACHED,messageQueueInitArguments.sharedMemPhysAddr);O(GSP_ARGUMENTS_CACHED,messageQueueInitArguments.pageTableEntryCount);O(GSP_ARGUMENTS_CACHED,messageQueueInitArguments.cmdQueueOffset);O(GSP_ARGUMENTS_CACHED,messageQueueInitArguments.statQueueOffset);O(GSP_ARGUMENTS_CACHED,srInitArguments.oldLevel);O(GSP_ARGUMENTS_CACHED,srInitArguments.flags);O(GSP_ARGUMENTS_CACHED,srInitArguments.bInPMTransition);O(GSP_ARGUMENTS_CACHED,gpuInstance);O(GSP_ARGUMENTS_CACHED,bDmemStack);O(GSP_ARGUMENTS_CACHED,profilerArgs.pa);O(GSP_ARGUMENTS_CACHED,profilerArgs.size);
 return 0;}
