# The Realtek NIC (RTL8111/8168 on the PRIME B450M-A II), docs/net/rtl8168.md. Two rungs of the ladder, same job:
#   scripts/metal-run.sh --kconf 'nic=probe' scripts/metal-jobs/nic-probe.sh   # read-only: XID, MAC, PHY status, register dump
#   scripts/metal-run.sh --kconf 'nic=reset' scripts/metal-jobs/nic-probe.sh   # + chip reset, auto-negotiation, wait for link
# The driver logs at boot, before each step; this job copies those lines into the run's log and fails unless the
# step the rung should reach is there. For `reset`, also check "link ... up" and that the MAC survived the reset.
echo "--- rtl8168 lines of the kernel log"
grep rtl8168 /proc/dmesg
echo "--- /proc/pci"
grep -i "10ec:8168\|rtl8168" /proc/pci
fail=0
need() { grep -q "$1" /proc/dmesg || { echo "nic-probe: MISSING: $1"; fail=1; }; }
need 'rtl8168: .* 10ec:8168 rev'
need 'rtl8168: BAR2 '
need 'rtl8168: TxConfig '
need 'rtl8168: register window (as found)'
if grep -q 'level Reset\|level Net' /proc/dmesg; then
  need 'rtl8168: reset done'
  need 'rtl8168: link '
  need 'rtl8168: mac after reset'
fi
exit $fail
