# The whole Realtek driver on the Ryzen (rings, DHCP, sockets), docs/net/rtl8168.md:
#   scripts/metal-run.sh --kconf 'nic=net' scripts/metal-jobs/nic-net.sh
# Needs the machine on a LAN with a DHCP server. Prints what it finds, one line per check, and fails on the first
# thing that does not work, so the log says how far a packet got: the lease (RX + TX), the router answering ICMP
# (raw sockets), DNS (UDP), an HTTP fetch (TCP). The driver's own lines (XID, MAC, link, register dumps) are copied too.
echo "--- rtl8168 lines of the kernel log"
grep rtl8168 /proc/dmesg
fail=0
step() { echo "nic-net: $*"; }
i=0
while ! grep -q nameserver /etc/resolv.conf && [ $i -lt 40 ]; do sleep 1; i=$((i + 1)); done
if grep -q nameserver /etc/resolv.conf; then
  step "DHCP lease after ${i}s: $(cat /etc/resolv.conf)"
else
  step "FAIL no DHCP lease in 40 s (RX or TX not working, or no DHCP server on the link)"
  exit 1
fi
ns=$(awk '/nameserver/ {print $2; exit}' /etc/resolv.conf)
if ping -c 3 "$ns" > /tmp/ping.out 2>&1; then step "ping $ns ok: $(grep 'packets received' /tmp/ping.out)"; else step "FAIL ping $ns"; cat /tmp/ping.out; fail=1; fi
if nslookup example.com "$ns" > /tmp/ns.out 2>&1 && grep -q 'Address.*[0-9]\.[0-9]' /tmp/ns.out; then step "DNS ok"; else step "FAIL DNS"; cat /tmp/ns.out; fail=1; fi
if timeout 30 wget -T 10 -q -O /tmp/page.html http://example.com/ 2>/tmp/wget.err && [ -s /tmp/page.html ]; then step "HTTP ok: $(wc -c < /tmp/page.html) bytes"; else step "FAIL HTTP"; cat /tmp/wget.err; fail=1; fi
udp_test > /tmp/udp.out 2>&1; grep -q 'udp_test: PASS' /tmp/udp.out && step "udp_test PASS" || { step "udp_test FAIL"; tail -n 5 /tmp/udp.out; fail=1; }
exit $fail
