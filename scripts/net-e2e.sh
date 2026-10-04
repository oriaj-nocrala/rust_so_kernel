#!/usr/bin/env bash
# End-to-end network check through the real userland (needs `socat`, `python3`).
#
#   scripts/net-e2e.sh [--no-build]
#
# Starts host peers (an echo server, an HTTP server with a 300 KB file, a
# server that never answers), boots the kernel headless with QEMU's user
# network plus a hostfwd into the guest, and types ONE command: `sh
# /mnt/e2e.sh` (disk-image-root/e2e.sh). The guest runs every check itself and
# reports on the serial log; the host only coordinates the two checks that
# need it (the guest listening, httpd) and enforces an overall deadline, so a
# hang is reported with the step it hung in instead of waiting for ever.
#
# Typing is slow (qemu-debug.sh paces keys), which is why the checks do not
# go through it one by one.
#
# Host ports: 47001 echo, 47010 http, 47020 silent, 47003 hostfwd -> guest 7777.
# NET_E2E_DEADLINE (seconds, default 240) bounds the guest run; NET_E2E_STEP_LIMIT
# (default 90) aborts early when one step makes no progress.
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DBG="$REPO/scripts/qemu-debug.sh"
LOG=/tmp/qemu-debug-rust_so_kernel/serial.log
DEADLINE="${NET_E2E_DEADLINE:-240}"
WORK="$(mktemp -d)"
FAILS=0
T0=$(date +%s)
PIDS=()
declare -A done_at
order=()

cleanup() {
    "$DBG" stop >/dev/null 2>&1
    for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
    rm -rf "$WORK"
}
trap cleanup EXIT

# ── host peers ──
cat > "$WORK/peers.py" <<'PY'
import socket, threading
def listener(port):
    ls = socket.socket(); ls.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    ls.bind(("127.0.0.1", port)); ls.listen(8)
    return ls
def echo(c):
    c.sendall(b"HELLO FROM HOST\n")
    while True:
        d = c.recv(8192)
        if not d: break
        c.sendall(d)
    c.shutdown(socket.SHUT_WR); c.close()
def serve_echo(ls):
    while True:
        c, _ = ls.accept(); threading.Thread(target=echo, args=(c,), daemon=True).start()
def serve_silent(ls):
    held = []
    while True:
        c, _ = ls.accept(); held.append(c)   # accepts, never answers
threading.Thread(target=serve_echo, args=(listener(47001),), daemon=True).start()
threading.Thread(target=serve_silent, args=(listener(47020),), daemon=True).start()
threading.Event().wait()
PY
mkdir -p "$WORK/www"
head -c 300000 /dev/urandom | base64 > "$WORK/www/big.txt"
md5sum "$WORK/www/big.txt" | cut -c1-32 > "$WORK/www/big.md5"
python3 "$WORK/peers.py" & PIDS+=($!)
(cd "$WORK/www" && exec python3 -m http.server 47010 --bind 127.0.0.1 >/dev/null 2>&1) & PIDS+=($!)
sleep 1

# ── boot, then one typed command ──
"$DBG" stop >/dev/null 2>&1
# disk.img only syncs disk-image-root/bin, so put the guest script on it here (QEMU is stopped).
debugfs -w -R "rm /e2e.sh" "$REPO/disk.img" >/dev/null 2>&1
debugfs -w -R "write $REPO/disk-image-root/e2e.sh /e2e.sh" "$REPO/disk.img" >/dev/null 2>&1
BUILD_FLAG=""; [ "${1:-}" = "--no-build" ] && BUILD_FLAG="--no-build"
QEMU_DEBUG_HOSTFWD="tcp:127.0.0.1:47003-:7777" "$DBG" start $BUILD_FLAG >/dev/null 2>&1
"$DBG" wait-for 'built-in shell' 300 >/dev/null 2>&1 || { echo "FAIL: no shell"; exit 1; }
echo "booted in $(( $(date +%s) - T0 ))s"
FROM=$(wc -l < "$LOG")
"$DBG" send "sh /mnt/e2e.sh" >/dev/null
"$DBG" enter >/dev/null
GUEST_T0=$(date +%s)

# ── coordinate until the guest is done or the deadline passes ──
# "name PASS|FAIL" for every result the guest has printed.
results() { tail -n +"$((FROM + 1))" "$LOG" | grep -ao 'E2E-RESULT [a-z_0-9]* [A-Z]*' | awk '{print $2, $3}'; }
seen() { tail -n +"$((FROM + 1))" "$LOG" | grep -q "$1"; }
HOST_SERVE=1; HOST_HTTPD=1
served=0; fetched=0
STEP_LIMIT="${NET_E2E_STEP_LIMIT:-90}"   # seconds one step may take before the run is aborted
cur_step=""; cur_since=$(date +%s)
while ! seen 'E2E-GUEST-DONE'; do
    now=$(date +%s)
    step=$(tail -n +"$((FROM + 1))" "$LOG" | grep -ao 'E2E-STEP [a-z_0-9]*' | tail -1)
    if [ "$step" != "$cur_step" ]; then cur_step="$step"; cur_since=$now; fi
    # Wall-clock timing from the host: the guest's own clocks are not reliable under TCG.
    while read -r name; do
        [ -n "$name" ] && [ -z "${done_at[$name]:-}" ] && done_at[$name]=$now && order+=("$name")
    done < <(tail -n +"$((FROM + 1))" "$LOG" | grep -ao 'E2E-RESULT [a-z_0-9]* ' | awk '{print $2}')
    if [ $((now - GUEST_T0)) -ge "$DEADLINE" ]; then
        echo "DEADLINE: the guest did not finish in ${DEADLINE}s; last step: ${cur_step:-none started}"
        FAILS=$((FAILS + 1)); break
    fi
    if [ $((now - cur_since)) -ge "$STEP_LIMIT" ]; then
        echo "HUNG: no progress for ${STEP_LIMIT}s in: ${cur_step:-before the first step}"
        FAILS=$((FAILS + 1)); break
    fi
    if [ $served = 0 ] && seen 'E2E-READY serve'; then
        served=1
        python3 - <<'PY'
import socket, sys, time
end = time.time() + 40
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
        HOST_SERVE=$?
    fi
    if [ $fetched = 0 ] && seen 'E2E-READY httpd'; then
        fetched=1
        body=$(python3 -c "
import urllib.request
try: print(urllib.request.urlopen('http://127.0.0.1:47003/hello.txt', timeout=8).read().decode()[:20])
except Exception as e: print('ERR', e)")
        echo "$body" | grep -q 'Hola desde /mnt'; HOST_HTTPD=$?
    fi
    sleep 0.5
done

# ── report ──
echo
prev=$GUEST_T0
for name in "${order[@]:-}"; do
    [ -z "$name" ] && continue
    res=$(results | awk -v n="$name" '$1 == n {print $2; exit}')
    printf "  %-5s %-16s %4ss\n" "$([ "$res" = PASS ] && echo ok || echo FAIL)" "$name" "$(( ${done_at[$name]} - prev ))"
    prev=${done_at[$name]}
done
fails=$(results | grep -c ' FAIL$')
FAILS=$((FAILS + fails))
if [ $served = 1 ] && [ $HOST_SERVE != 0 ]; then echo "  FAIL  host client of the guest listener"; FAILS=$((FAILS + 1)); fi
if [ $fetched = 1 ]; then
    if [ $HOST_HTTPD = 0 ]; then echo "  ok    httpd served /mnt/hello.txt to the host"; else echo "  FAIL  httpd served /mnt/hello.txt to the host"; FAILS=$((FAILS + 1)); fi
else
    echo "  FAIL  httpd was never reached"; FAILS=$((FAILS + 1))
fi
echo "guest ran $(( $(date +%s) - GUEST_T0 ))s, total $(( $(date +%s) - T0 ))s"
if [ "$FAILS" = 0 ]; then echo "net-e2e: PASS"; else echo "net-e2e: FAIL ($FAILS)"; fi
exit $((FAILS != 0))
