# Normal-use checks of the Realtek driver on the Ryzen, plus a cable pull (docs/net/rtl8168.md):
#   scripts/metal-run.sh --kconf 'nic=net' scripts/metal-jobs/nic-soak.sh
# Run it after nic-net.sh passes. Phase 1 repeats the basics so a flake shows (30 pings, 10 HTTP fetches, 10 DNS
# lookups, the socket tests twice), phase 2 asks YOU to unplug the cable and plug it back (watch the screen: it says
# when, and waits up to 60 s each way), then repeats ping/DNS/HTTP to show the link and the lease came back.
# Prints one line per check and keeps going after a failure; the exit code is 1 if anything failed.
fail=0
step() { echo "nic-soak: $*"; }
nicline() { grep "$1" /proc/nic; }
i=0
while ! nicline '^lease: [0-9]' > /dev/null && [ $i -lt 60 ]; do sleep 1; i=$((i + 1)); done
if ! nicline '^lease: [0-9]' > /dev/null; then step "FAIL no DHCP lease in 60 s"; cat /proc/nic; exit 1; fi
step "lease after ${i}s: $(nicline '^lease:')"
router=$(nicline '^lease:' | sed 's/.*router Some(\([0-9.]*\)).*/\1/')

checks() { # $1 = label
  if ping -c 5 "$router" > /tmp/ping.out 2>&1; then step "$1 ping ok: $(grep 'packets received' /tmp/ping.out)"; else step "$1 FAIL ping"; cat /tmp/ping.out; fail=1; fi
  if nslookup example.com "$router" > /tmp/ns.out 2>&1 && grep -q 'Address.*[0-9]\.[0-9]' /tmp/ns.out; then step "$1 DNS ok"; else step "$1 FAIL DNS"; cat /tmp/ns.out; fail=1; fi
  if timeout 30 wget -T 10 -q -O /tmp/page.html http://example.com/ 2>/tmp/wget.err && [ -s /tmp/page.html ]; then step "$1 HTTP ok: $(wc -c < /tmp/page.html) bytes"; else step "$1 FAIL HTTP"; cat /tmp/wget.err; fail=1; fi
}

echo "=== phase 1: repeated basics"
if ping -c 30 "$router" > /tmp/ping.out 2>&1; then step "30 pings: $(grep 'packets received' /tmp/ping.out)"; else step "FAIL 30 pings"; tail -n 4 /tmp/ping.out; fail=1; fi
ok=0; n=0
while [ $n -lt 10 ]; do
  n=$((n + 1))
  timeout 30 wget -T 10 -q -O /tmp/page.html http://example.com/ 2>/dev/null && [ -s /tmp/page.html ] && ok=$((ok + 1))
done
[ $ok -eq 10 ] && step "HTTP 10/10" || { step "FAIL HTTP $ok/10"; fail=1; }
ok=0; n=0
while [ $n -lt 10 ]; do
  n=$((n + 1))
  nslookup example.com "$router" > /tmp/ns.out 2>&1 && grep -q 'Address.*[0-9]\.[0-9]' /tmp/ns.out && ok=$((ok + 1))
done
[ $ok -eq 10 ] && step "DNS 10/10" || { step "FAIL DNS $ok/10"; fail=1; }
for t in 1 2; do
  udp_test "$router" > /tmp/udp.out 2>&1; grep -q 'udp_test: PASS' /tmp/udp.out && step "udp_test PASS (run $t)" || { step "udp_test FAIL (run $t)"; tail -n 5 /tmp/udp.out; fail=1; }
  icmp_test "$router" > /tmp/icmp.out 2>&1; grep -q 'icmp_test: PASS' /tmp/icmp.out && step "icmp_test PASS (run $t)" || { step "icmp_test FAIL (run $t)"; tail -n 5 /tmp/icmp.out; fail=1; }
done
echo "--- /proc/nic after phase 1"
cat /proc/nic

echo "=== phase 2: cable pull"
before=$(nicline 'link changes' | sed 's/.*link changes \([0-9]*\).*/\1/')
step ">>> UNPLUG THE ETHERNET CABLE NOW (waiting up to 60 s) <<<"
i=0
while ! nicline 'PHYstatus' | grep -q 'up: false' && [ $i -lt 60 ]; do sleep 1; i=$((i + 1)); done
if nicline 'PHYstatus' | grep -q 'up: false'; then step "link down seen after ${i}s"; else step "FAIL link never went down (cable not pulled, or the PHY status is stuck)"; fail=1; fi
step "wait 5 s, then >>> PLUG IT BACK <<< (waiting up to 60 s)"
sleep 5
i=0
while ! nicline 'PHYstatus' | grep -q 'up: true' && [ $i -lt 60 ]; do sleep 1; i=$((i + 1)); done
if nicline 'PHYstatus' | grep -q 'up: true'; then step "link up again after ${i}s"; else step "FAIL link never came back"; fail=1; fi
after=$(nicline 'link changes' | sed 's/.*link changes \([0-9]*\).*/\1/')
[ "$after" -ge $((before + 2)) ] && step "link changes $before -> $after (down and up counted)" || { step "FAIL link changes $before -> $after (expected at least +2)"; fail=1; }
i=0
while ! nicline '^lease: [0-9]' > /dev/null && [ $i -lt 60 ]; do sleep 1; i=$((i + 1)); done
nicline '^lease: [0-9]' > /dev/null && step "lease back ${i}s after link up: $(nicline '^lease:')" || { step "FAIL no lease after the cable came back"; fail=1; }
sleep 3
checks "after replug"
echo "--- rtl8168 lines of the kernel log"
grep rtl8168 /proc/dmesg | tail -n 12
echo "--- /proc/nic at the end"
cat /proc/nic
exit $fail
