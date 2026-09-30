# G1 slices 19-20, the LAPIC one-shot timer (e49df20) and the tokio probes, on the Ryzen: never measured on metal.
#   cp probes/tokio/target/x86_64-unknown-linux-musl/release/tk{probe,stress} disk-image-root/bin/   # then a normal build
#   scripts/metal-run.sh scripts/metal-jobs/abi-timer.sh
# Runs every C ABI test (the list in /mnt/abi-suite.sh) one by one, then tkprobe and tkstress (tokio: timers, multi_thread
# runtime, AF_UNIX, fs, process, signals, then 10k tasks / 1000 timers / 32 MiB through a socket / ...). Prints only a summary
# (the log wraps): the last lines of any test that failed, and every TK line that says ok=false or DONE.
fail=0
sum() { echo "$*"; echo "$*" >> /tmp/abi-timer.sum; }
: > /tmp/abi-timer.sum
tests=$(sed -n '/^    tests="/,/^fi/p' /mnt/abi-suite.sh | sed -e 's/^    tests="//' -e 's/"$//' -e '/^fi/d' | tr '\n' ' ')
total=0; bad=0
for t in $tests; do
  total=$((total + 1))
  /mnt/bin/$t > /tmp/t.out 2>&1
  rc=$?
  if [ $rc != 0 ] || grep -q FAIL /tmp/t.out; then
    bad=$((bad + 1))
    sum "abi: $t exit=$rc"
    grep -E 'FAIL|fail|error' /tmp/t.out | head -4 | while read l; do sum "abi:   $l"; done
    tail -2 /tmp/t.out | while read l; do sum "abi:   | $l"; done
  fi
done
sum "abi: $total tests, $bad bad"
[ "$total" -ge 38 ] || { sum "abi: expected at least 38 tests"; fail=1; }
[ "$bad" = 0 ] || fail=1

# tokio: current_thread + multi_thread runtimes, timers, then the heavy one.
for p in tkprobe tkstress; do
  s=$(cut -d' ' -f1 /proc/uptime)
  /mnt/bin/$p > /tmp/$p.out 2>&1
  rc=$?
  e=$(cut -d' ' -f1 /proc/uptime)
  el=$(awk -v a="$s" -v b="$e" 'BEGIN { printf "%.1f", b - a }')
  nb=$(grep -c 'ok=false' /tmp/$p.out)
  done_=$(grep -c 'TK DONE' /tmp/$p.out)
  sum "tokio: $p exit=$rc ${el}s ok=false:$nb done:$done_"
  grep 'ok=false' /tmp/$p.out | head -5 | while read l; do sum "tokio:   $l"; done
  [ "$rc" = 0 ] && [ "$nb" = 0 ] && [ "$done_" = 1 ] || { fail=1; tail -3 /tmp/$p.out | while read l; do sum "tokio:   | $l"; done; }
done

echo "---- summary (the log wraps) ----"
cat /tmp/abi-timer.sum
sum "abi-timer: verdict exit=$fail"
exit $fail
