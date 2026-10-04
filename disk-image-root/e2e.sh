#!/bin/sh
# Guest half of scripts/net-e2e.sh: runs every network check inside the guest and prints
#   E2E-STEP <name>              before a step (so a hang names itself)
#   E2E-RESULT <name> PASS|FAIL (the host times the steps: the guest's clocks are not reliable under TCG)
#   E2E-READY <what>             when the host has to do its part (serve, httpd)
#   E2E-GUEST-DONE               at the end
# Every command runs under `timeout`, so one stuck step cannot stall the rest.
# Host peers (see net-e2e.sh): echo 10.0.2.2:47001, http 10.0.2.2:47010, a silent
# server 10.0.2.2:47020, and hostfwd host:47003 -> guest:7777.
H=10.0.2.2


# check <name> <seconds> <pattern> <command...>: PASS if the output matches.
check() {
    name=$1; limit=$2; pat=$3; shift 3
    echo "E2E-STEP $name"
    out=$(timeout "$limit" "$@" 2>&1)
    if echo "$out" | grep -q "$pat"; then r=PASS; else r=FAIL; fi
    echo "E2E-RESULT $name $r"
    [ "$r" = FAIL ] && echo "$out" | tail -n 6
}

echo "E2E-STEP dhcp"
i=0
while ! grep -q nameserver /etc/resolv.conf && [ $i -lt 40 ]; do sleep 1; i=$((i + 1)); done
if grep -q 'nameserver 10.0.2.3' /etc/resolv.conf; then r=PASS; else r=FAIL; fi
echo "E2E-RESULT dhcp $r"

check udp_test 30 'udp_test: PASS' udp_test
check icmp_test 30 'icmp_test: PASS' icmp_test
check ping 20 '3 packets received' ping -c 3 $H
check tcp_test 40 'tcp_test: PASS' tcp_test
check nc 20 'hi-from-nc' sh -c "echo hi-from-nc | nc $H 47001"
check nslookup 20 'Address.*[0-9]\.[0-9]' nslookup example.com 10.0.2.3
check itimer_test 30 'itimer_test: PASS' itimer_test
check wget_timeout 20 'download timed out' wget -T 3 -O - http://$H:47020/

# 300 KB over HTTP; the host serves the file and its md5.
echo "E2E-STEP wget_md5"
wget -q -O /tmp/big.txt http://$H:47010/big.txt
want=$(wget -q -O - http://$H:47010/big.md5 | cut -c1-32)
have=$(md5sum /tmp/big.txt | cut -c1-32)
if [ -n "$want" ] && [ "$want" = "$have" ]; then r=PASS; else r=FAIL; fi
echo "E2E-RESULT wget_md5 $r"

# The host connects through hostfwd once it sees E2E-READY serve.
echo "E2E-READY serve"
check tcp_serve 60 'tcp_test: PASS' tcp_test serve

# The host fetches /mnt/hello.txt through hostfwd once it sees E2E-READY httpd.
echo "E2E-STEP httpd"
httpd -p 7777 -h /mnt &
sleep 1
echo "E2E-READY httpd"
sleep 8
kill $! 2>/dev/null
echo "E2E-GUEST-DONE"
