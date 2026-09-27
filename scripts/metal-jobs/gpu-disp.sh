# Phase 2 of docs/gpu/gpu-plan.md, on the Ryzen:
#   scripts/metal-run.sh --kconf 'gpu=disp' scripts/metal-jobs/gpu-disp.sh
# Prints /proc/gpu and /proc/displays into the log and fails unless:
# - the VBIOS read from the PROM matches the phase 0 dump (FNV-1a 64 of the
#   first 148 992 bytes = static/vbios-rom.bin) and its version;
# - the DCB outputs are the ones nouveau printed (trace-nogsp dmesg);
# - the ASUS (DP-3, AUX 3) and the HP (HDMI-A-1, I2C port 5) are connected
#   and named, and their EDIDs are byte-identical to sysfs's
#   (nvgpu/fixtures/edid-*.bin, hex embedded below).
cat /proc/gpu
cat /proc/displays
fail=0
need() { grep -q "$2" "$1" || { echo "gpu-disp: MISSING in $1: $2"; fail=1; }; }
need /proc/gpu 'arch GA100 impl 6 chip GA106'
need /proc/gpu 'fnv1a64\[\.\.148992\] 0aa0e0978856d1d3'
need /proc/gpu 'vbios: version 94.06.37.00.40'
need /proc/gpu 'dcb: outp 04 type 06 loc 0 or 2 link 2 con 2 edid 6 bus 2 head f'
need /proc/gpu 'dcb: outp 07 type 02 loc 0 or 2 link 1 con 3 edid 5 bus 3 head f'
need /proc/displays '^DP-3 *connected *conn 2 aux 3  AUS VG279Q3A$'
need /proc/displays '^HDMI-A-1 *connected *conn 3 i2c 5  HWP HP 2309$'
need /proc/displays 'dpcd: 1.4 HBR2 x4'
need /proc/displays 'edid-fnv1a64: 79d18ea46cd40a9f'
need /proc/displays 'edid-fnv1a64: d556fc44c2071939'
need /proc/displays 'edid-hex: 00ffffffffffff0006b352270000000032200104a53c22783b7285a6554fa0260d5054bfef00d1cf818081c0814081009500b300714f023a801871382d40582c450055502100001e000000fd0030b4fafa2b010a202020202020000000fc0056473237395133410a20202020000000ff0053314c4d54463134313036300a015c020331f156013f050302044e90401f1312110f0e1e1d140716061523090707830100006d1a0000020130b40000000000005a8780a070384d403020350055502100001aa49c80a0703859403020350055502100001a5ea480a070382d403020280055502100001a000000000000000000000000000000000000000000000000c2$'
need /proc/displays 'edid-hex: 00ffffffffffff0022f02328010101013313010380331d78eeee95a3544c99260f5054a10800814081809500a940b300d1c001010101023a801871382d40582c4500fe1f1100001e000000fd00304c185e11000a202020202020000000fc00485020323330390a2020202020000000ff00334351393531303438300a2020014b020329f1230907074f84020301060715161112101f131405830100006c030c001000b82dc0010101018c0ad08a20e02d10103e9600fe1f110000188c0ad090204031200c405500fe1f11000018011d8018711c1620582c2500fe1f1100009e011d80d0721c1620102c2580fe1f1100009e0000000000000000000000000000ae$'
exit $fail
