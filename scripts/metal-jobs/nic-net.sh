# The whole Realtek driver on the Ryzen (rings, DHCP, sockets), docs/net/rtl8168.md:
#   scripts/metal-run.sh --kconf 'nic=net' scripts/metal-jobs/nic-net.sh
# Needs the machine on a LAN with a DHCP server. Prints what it finds, one line per check, and keeps going after a
# failure so one run says as much as possible: the lease (RX + TX), the router answering ICMP (raw sockets), DNS
# (UDP), an HTTP fetch (TCP). `/proc/nic` (counters, registers that say whether packets move, ring state, the
# first bytes of the last frames each way) is printed at the start, after the DHCP wait and at the end: with no lease
# it is the evidence of what the chip did. The driver's own boot lines are copied too.
echo "--- rtl8168 lines of the kernel log"
grep rtl8168 /proc/dmesg
echo "--- /proc/nic right after boot"
cat /proc/nic
fail=0
step() { echo "nic-net: $*"; }
i=0
while ! grep -q nameserver /etc/resolv.conf && [ $i -lt 60 ]; do sleep 1; i=$((i + 1)); done
echo "--- /proc/nic after waiting ${i}s for the lease"
cat /proc/nic
if grep -q nameserver /etc/resolv.conf; then
  step "DHCP lease after ${i}s: $(cat /etc/resolv.conf)"
else
  step "FAIL no DHCP lease in 60 s (RX or TX not working, or no DHCP server on the link; read /proc/nic above: 'tx N' rising with 'rx 0' = frames leave but nothing comes back; 'tx 0' = the stack never sent; 'events seen' shows SysErr/RxOverflow)"
  exit 1
fi
ns=$(awk '/nameserver/ {print $2; exit}' /etc/resolv.conf)
if ping -c 3 "$ns" > /tmp/ping.out 2>&1; then step "ping $ns ok: $(grep 'packets received' /tmp/ping.out)"; else step "FAIL ping $ns"; cat /tmp/ping.out; fail=1; fi
if nslookup example.com "$ns" > /tmp/ns.out 2>&1 && grep -q 'Address.*[0-9]\.[0-9]' /tmp/ns.out; then step "DNS ok"; else step "FAIL DNS"; cat /tmp/ns.out; fail=1; fi
if timeout 30 wget -T 10 -q -O /tmp/page.html http://example.com/ 2>/tmp/wget.err && [ -s /tmp/page.html ]; then step "HTTP ok: $(wc -c < /tmp/page.html) bytes"; else step "FAIL HTTP"; cat /tmp/wget.err; fail=1; fi
udp_test "$ns" > /tmp/udp.out 2>&1; grep -q 'udp_test: PASS' /tmp/udp.out && step "udp_test PASS" || { step "udp_test FAIL"; tail -n 5 /tmp/udp.out; fail=1; }
icmp_test "$ns" > /tmp/icmp.out 2>&1; grep -q 'icmp_test: PASS' /tmp/icmp.out && step "icmp_test PASS" || { step "icmp_test FAIL"; tail -n 5 /tmp/icmp.out; fail=1; }
echo "--- /proc/nic at the end"
cat /proc/nic
exit $fail
