/*
 * Symbols NVK's shared code references but a constanos process never reaches: the runtime's DRM device enumeration (the constanos
 * physical device comes from NVK's own enumerate callback), DRM display WSI and udev hotplug. There is no DRM here.
 * Each stub returns "failure" (-1 / NULL) and says so; none is expected to run.
 */
#include <stdint.h>
#include <stdio.h>

#define STUB(name) long name(void) { fprintf(stderr, "constanos: %s is a stub (no DRM/udev here)\n", #name); return -1; }

STUB(drmAuthMagic)
STUB(drmDropMaster)
STUB(drmFreeDevices)
STUB(drmGetDevices2)
STUB(drmIoctl)
STUB(drmModeAddFB2WithModifiers)
STUB(drmModeConnectorSetProperty)
STUB(drmModeFreeResources)
STUB(drmModeGetResources)
STUB(drmModeRmFB)
STUB(drmPrimeFDToHandle)
STUB(drmSetClientCap)
STUB(drmSetMaster)
STUB(drmSyncobjFDToHandle)
STUB(udev_device_unref)
STUB(udev_monitor_enable_receiving)
STUB(udev_monitor_filter_add_match_subsystem_devtype)
STUB(udev_monitor_get_fd)
STUB(udev_monitor_new_from_netlink)
STUB(udev_new)
STUB(drmCrtcGetSequence)
STUB(drmCrtcQueueSequence)
STUB(drmFreeDevice)
STUB(drmGetDevice2)
STUB(drmModeFreeConnector)
STUB(drmModeFreeCrtc)
STUB(drmModeFreeEncoder)
STUB(drmModeFreeObjectProperties)
STUB(drmModeGetConnector)
STUB(drmModeGetConnectorCurrent)
STUB(drmModeGetCrtc)
STUB(drmModeGetEncoder)
STUB(drmModeGetPlane)
STUB(drmModeObjectGetProperties)
STUB(drmSyncobjDestroy)
STUB(drmSyncobjSignal)
STUB(udev_device_get_property_value)
STUB(udev_monitor_receive_device)
STUB(udev_monitor_unref)
STUB(udev_unref)
STUB(di_cta_data_block_get_colorimetry)
STUB(di_cta_data_block_get_hdr_static_metadata)
STUB(di_edid_cta_get_data_blocks)
STUB(di_edid_ext_get_cta)
STUB(di_edid_get_chromaticity_coords)
STUB(di_edid_get_extensions)
STUB(di_edid_get_screen_size)
STUB(di_info_destroy)
STUB(di_info_get_edid)
STUB(di_info_get_failure_msg)
STUB(di_info_get_make)
STUB(di_info_get_model)
STUB(di_info_parse_edid)
STUB(drmModeFreePlane)
STUB(drmModeFreePlaneResources)
STUB(drmModeFreeProperty)
STUB(drmModeFreePropertyBlob)
STUB(drmModeGetPlaneResources)
STUB(drmModeGetProperty)
STUB(drmModeGetPropertyBlob)
STUB(drmHandleEvent)
STUB(drmIsMaster)
STUB(drmModeAtomicAddProperty)
STUB(drmModeAtomicAlloc)
STUB(drmModeAtomicCommit)
STUB(drmModeAtomicFree)
STUB(drmModeCreatePropertyBlob)
STUB(spvBinaryToText)
STUB(spvContextCreate)
STUB(spvDiagnosticDestroy)
STUB(spvDiagnosticPrint)
STUB(spvTextDestroy)

STUB(elf64_getehdr)
STUB(elf64_getshdr)
STUB(elf_end)
STUB(elf_errmsg)
STUB(elf_errno)
STUB(elf_getdata)
STUB(elf_getscn)
STUB(elf_getshdrstrndx)
STUB(elf_kind)
STUB(elf_memory)
STUB(elf_nextscn)
STUB(elf_strptr)
STUB(elf_version)

/*
 * Two glibc-only names that gcc's libstdc++.a (built for glibc) references. They are real functions, not stubs: musl has the
 * ordinary versions.
 */
#include <stdarg.h>
#include <stdlib.h>

unsigned long __isoc23_strtoul(const char *s, char **end, int base) { return strtoul(s, end, base); }

int __sprintf_chk(char *buf, int flag, size_t len, const char *fmt, ...) {
   (void)flag;
   va_list ap;
   va_start(ap, fmt);
   int n = vsnprintf(buf, len, fmt, ap);
   va_end(ap);
   return n;
}
