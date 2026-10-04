#!/usr/bin/env bash
# End-to-end network check through the real userland (needs `socat`, `python3`).
#
#   scripts/net-e2e.sh [--no-build]
#
# Starts host peers (an echo server, an HTTP server with a 300 KB file),
# boots the kernel headless with QEMU's user network plus a hostfwd into the
# guest, then drives the guest shell: udp_test, tcp_test (client and serve),
# wget (md5 of a big file), nc, nslookup, and httpd reached from the host.
# Prints one line per check and exits nonzero if any fails.
#
# Host ports used: 47001 (echo), 47010 (http), 47003 (hostfwd -> guest 7777).
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DBG="$REPO/scripts/qemu-debug.sh"
LOG=/tmp/qemu-debug-rust_so_kernel/serial.log
WORK="$(mktemp -d)"
FAILS=0

cleanup() {
    "$DBG" stop >/dev/null 2>&1
    [ -n "${ECHO_PID:-}" ] && kill "$ECHO_PID" 2>/dev/null
    [ -n "${HTTP_PID:-}" ] && kill "$HTTP_PID" 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT

report() { # ok? name
    if [ "$1" = 1 ]; then echo "  ok    $2"; else echo "  FAIL  $2"; FAILS=$((FAILS + 1)); fi
}

# Runs a guest command and waits for it to finish (an upper-case marker the
# typed command line, which is lower case, cannot match; unique per call, since
# this runs in a subshell). Output: the lines the guest printed, from the log.
guest() { # timeout command...
    local timeout="$1"; shift
    local id
    id=$(date +%s%N)
    local mark="NETE2E-DONE-$id"
    local from
    from=$(wc -l < "$LOG")
    "$DBG" send "$* ; echo nete2e-done-$id | tr a-z A-Z" >/dev/null
    "$DBG" enter >/dev/null
    "$DBG" wait-for "$mark" "$timeout" >/dev/null 2>&1
    # Not only [fb] lines: output that follows an unterminated line has no prefix.
    tail -n +"$((from + 1))" "$LOG" | sed 's/^\[fb\] //'
}

# ── host peers ──
cat > "$WORK/echo.py" <<'PY'
import socket, sys, threading
ls = socket.socket(); ls.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
ls.bind(("127.0.0.1", 47001)); ls.listen(8)
def h(c):
    c.sendall(b"HELLO FROM HOST\n")
    while True:
        d = c.recv(8192)
        if not d: break
        c.sendall(d)
    c.shutdown(socket.SHUT_WR); c.close()
while True:
    c, _ = ls.accept(); threading.Thread(target=h, args=(c,), daemon=True).start()
PY
mkdir -p "$WORK/www"
head -c 300000 /dev/urandom | base64 > "$WORK/www/big.txt"
echo "hello from the host http server" > "$WORK/www/index.html"
python3 "$WORK/echo.py" & ECHO_PID=$!
(cd "$WORK/www" && exec python3 -m http.server 47010 --bind 127.0.0.1 >/dev/null 2>&1) & HTTP_PID=$!
sleep 1
HOST_MD5=$(md5sum "$WORK/www/big.txt" | cut -c1-32)

# ── boot ──
"$DBG" stop >/dev/null 2>&1
BUILD_FLAG=""; [ "${1:-}" = "--no-build" ] && BUILD_FLAG="--no-build"
QEMU_DEBUG_HOSTFWD="tcp:127.0.0.1:47003-:7777" "$DBG" start $BUILD_FLAG >/dev/null 2>&1
"$DBG" wait-for 'built-in shell' 300 >/dev/null 2>&1 || { echo "FAIL: no shell"; exit 1; }
sleep 15

out=$(guest 60 'cat /etc/resolv.conf')
echo "$out" | grep -q 'nameserver 10.0.2.3'; report $((! $?)) "DHCP lease reaches /etc/resolv.conf"

out=$(guest 120 udp_test)
echo "$out" | grep -q 'udp_test: PASS'; report $((! $?)) "udp_test (UDP sockets, DNS round trip)"

out=$(guest 120 tcp_test)
echo "$out" | grep -q 'tcp_test: PASS'; report $((! $?)) "tcp_test (TCP client)"

out=$(guest 60 'echo hi-from-nc | nc 10.0.2.2 47001')
echo "$out" | grep -q 'hi-from-nc'; report $((! $?)) "nc to the host echo server"

out=$(guest 60 'nslookup example.com 10.0.2.3')
echo "$out" | grep -q 'Address.*[0-9]\+\.[0-9]\+\.[0-9]\+\.[0-9]\+'; report $((! $?)) "nslookup resolves a name"

out=$(guest 120 'wget -q -O /tmp/big.txt http://10.0.2.2:47010/big.txt; md5sum /tmp/big.txt')
echo "$out" | grep -q "$HOST_MD5"; report $((! $?)) "wget of 300 KB matches the host's md5"

# tcp_test serve: the guest listens, the host connects through hostfwd.
serve_from=$(wc -l < "$LOG")
"$DBG" send "tcp_test serve" >/dev/null; "$DBG" enter >/dev/null
sleep 3
python3 - <<'PY'
import socket, sys, time
end = time.time() + 60
while time.time() < end:
    try:
        c = socket.create_connection(("127.0.0.1", 47003), timeout=3)
        c.sendall(b"ping from host"); c.settimeout(5)
        d = b""
        while len(d) < 4:
            x = c.recv(16)
            if not x: break
            d += x
        c.close()
        if d == b"pong": sys.exit(0)
    except Exception:
        pass
    time.sleep(0.3)
sys.exit(1)
PY
host_ok=$?
serve_ok=1
for _ in $(seq 30); do # only output newer than this step counts
    if tail -n +"$((serve_from + 1))" "$LOG" | grep -q 'tcp_test: PASS'; then serve_ok=0; break; fi
    sleep 1
done
[ $host_ok = 0 ] && [ $serve_ok = 0 ]; report $((! $?)) "tcp_test serve (listen/accept from the host)"

"$DBG" send "httpd -p 7777 -h /mnt &" >/dev/null; "$DBG" enter >/dev/null
sleep 5
body=$(python3 -c "
import urllib.request
try: print(urllib.request.urlopen('http://127.0.0.1:47003/hello.txt', timeout=15).read().decode()[:20])
except Exception as e: print('ERR', e)")
echo "$body" | grep -q 'Hola desde /mnt'; report $((! $?)) "httpd serves a file from /mnt to the host"

echo
if [ "$FAILS" = 0 ]; then echo "net-e2e: PASS"; else echo "net-e2e: FAIL ($FAILS)"; fi
exit $((FAILS != 0))
