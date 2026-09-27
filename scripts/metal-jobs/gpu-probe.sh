# Phase 1 of docs/gpu/gpu-plan.md, on the Ryzen:
#   scripts/metal-run.sh --kconf 'gpu=probe' scripts/metal-jobs/gpu-probe.sh
# Prints /proc/gpu into the log and fails unless it shows what the phase's
# criterion asks for: the GPU's MSI capability, PMC_BOOT_0 decoded as
# GA100-family impl 6 (GA106), the three firmware files byte-identical to the
# host's (FNV-1a 64 of disk-image-root/lib/firmware/..., 570.144), and D4
# (IommuEn=0).
cat /proc/gpu
fail=0
need() { grep -q "$1" /proc/gpu || { echo "gpu-probe: MISSING: $1"; fail=1; }; }
need 'device: 09:00.0 10de:2507'
need 'msi: @68 .*64bit=true'
need 'arch GA100 impl 6 chip GA106'
need 'bar1: .*mapped WC'
need 'bootloader-570.144.bin 24684 bytes.*fnv1a64 9d6364f6c7adf438'
need 'booter_load-570.144.bin 61304 bytes.*fnv1a64 e5b13c4de1094144'
need 'booter_unload-570.144.bin 41080 bytes.*fnv1a64 59fccc245c86554b'
need 'IommuEn=0'
cat /proc/pci | grep -i nvgpu
exit $fail
