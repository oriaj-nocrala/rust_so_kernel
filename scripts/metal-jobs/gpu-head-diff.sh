# What head 1 (lit by `hdmi on`: this driver programs its whole state, and the cursor enables on it) has that head 0 (the GOP's) lacks: the core's ARMED state for the
# head methods 0x2000-0x23fc of head 0 against the same +0x400 of head 1, as `H0=` / `H1=` pairs for offline diffing (ARMED base 0x688000), plus window 0 and 2 (0x1000..0x10fc
# and 0x1100..0x11fc). The job passes if it could read them.
#   touch build.rs; echo 3 > target/metal/budget; scripts/metal-run.sh --kconf 'gpu=hdmi' scripts/metal-jobs/gpu-head-diff.sh
sumfile=/tmp/gpu-head-diff.sum
fail=0
sum() { echo "$*"; echo "$*" >> $sumfile; }
: > $sumfile
sum "gpu-head-diff: start"
[ -e /dev/dispctl ] || { sum "gpu-head-diff: no /dev/dispctl"; echo "---- summary (the log wraps) ----"; cat $sumfile; exit 1; }
echo "hdmi on" > /dev/dispctl && sum "gpu-head-diff: hdmi on ok" || { sum "gpu-head-diff: hdmi on FAILED"; fail=1; }
sleep 2
dump() { # dump <label> <first offset> <last offset> <stride between the two rows>
  o=$2
  while [ $o -le $3 ]; do
    n=0
    while [ $n -lt 24 ] && [ $o -le $3 ]; do
      a=$((0x688000 + o)); b=$((a + $4))
      echo "peek $a" > /dev/dispctl; echo "peek $b" > /dev/dispctl
      o=$((o + 4)); n=$((n + 1))
    done
    sum "gpu-head-diff: $1 $(grep -a '^peek:' /dev/dispctl | cut -c1-1200)"
  done
}
dump "head" $((0x2000)) $((0x20fc)) $((0x400))
dump "head" $((0x2100)) $((0x21fc)) $((0x400))
dump "head" $((0x2200)) $((0x22fc)) $((0x400))
dump "head" $((0x2300)) $((0x23fc)) $((0x400))
dump "window" $((0x1000)) $((0x10fc)) $((0x100))
echo "---- summary (the log wraps) ----"
cat $sumfile
sum "gpu-head-diff: verdict exit=$fail"
exit $fail
