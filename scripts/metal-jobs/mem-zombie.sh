# Zombies give their memory back at exit, no address space is freed under SCHEDULER, and the ext2 block cache is sized to the machine.
#   touch build.rs; scripts/metal-run.sh scripts/metal-jobs/mem-zombie.sh
# 1. the kernel's choice of cache size on this RAM (`ext2: block cache up to N MiB` in the boot log, `max_mib=` in /proc/kdebug);
# 2. the process tests that touch exit, wait, fork, threads and signals, on real hardware (zombie_mem_test is the one that measures
#    MemFree between a child's exit and its wait);
# 3. the cache with real Vulkan binaries off the USB stick: every 15-19 MB program read once (cold, from the stick), then again — a
#    cache that can hold them all adds no device reads the second time (the old fixed 32 MiB held about two of them), and the second
#    pass is much faster than the first; one that cannot (QEMU's 56 MiB) is only reported;
# 4. `space_frees_under_lock=0` in /proc/kdebug at the end (an address space freed with SCHEDULER held counts there and warns on screen).
fail=0
sum() { echo "$*"; echo "$*" >> /tmp/mem-zombie.sum; }
: > /tmp/mem-zombie.sum
field() { echo "$1" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2; }
now() { cut -d' ' -f1 /proc/uptime; }
sum "mem-zombie: $(grep MemTotal /proc/meminfo | tr -s ' ') $(grep MemFree /proc/meminfo | tr -s ' ')"
sum "mem-zombie: boot: $(grep -a 'ext2: block cache' /proc/dmesg | head -n 2 | tr '\n' ' ')"
c0=$(grep '^ext2_cache:' /proc/kdebug)
sum "mem-zombie: cache at start: $c0"

for t in zombie_mem_test lifecycle_test fork_exec_test vfork_test exitgroup_test pthread_test wait_intr_test jobctl_test; do
  t0=$(now)
  /mnt/bin/$t > /tmp/$t.out 2>&1
  rc=$?
  sum "mem-zombie: $t exit=$rc ($t0 .. $(now) s)"
  [ $rc = 0 ] || { fail=1; grep -a 'FAIL' /tmp/$t.out | head -n 8 >> /tmp/mem-zombie.sum; }
done
grep -a 'MemFree:' /tmp/zombie_mem_test.out | while read -r l; do sum "mem-zombie: zombie_mem_test: $l"; done

big="/mnt/bin/vk_comp /mnt/bin/vk_window /mnt/bin/vk_draw /mnt/bin/vk_probe /mnt/bin/vk_share /mnt/bin/snake3d"
total=$(ls -l $big | awk '{s += $5} END {print int(s / 1048576)}')
sum "mem-zombie: the six programs are $total MiB"
for pass in 1 2 3; do
  a=$(grep '^ext2_cache:' /proc/kdebug)
  t0=$(now)
  cat $big > /dev/null
  t1=$(now)
  b=$(grep '^ext2_cache:' /proc/kdebug)
  dr=$(( $(field "$b" device_reads) - $(field "$a" device_reads) ))
  dk=$(( $(field "$b" device_kib) - $(field "$a" device_kib) ))
  sum "mem-zombie: pass $pass of cat: $t0 .. $t1 s, $dr device reads, $dk KiB from the stick, held $(field "$b" held_mib)/$(field "$b" max_mib) MiB"
  # Only a cache that can hold them all is owed a second pass with no reads (a cyclic scan bigger than the cache misses on
  # everything, whatever the policy: QEMU's 56 MiB does that, this machine's should not).
  if [ $pass -ge 2 ] && [ "$(field "$b" max_mib)" -ge $((total + 16)) ]; then
    [ $dr = 0 ] || { sum "mem-zombie: pass $pass still read the stick ($dr reads) with room for $total MiB in $(field "$b" max_mib)"; fail=1; }
  fi
done

# MemFree after everything (the cache holds what it read, so it is lower than at the start by about what it holds)
m=$(grep MemFree /proc/meminfo | tr -s ' ')
sum "mem-zombie: end: $m"
s=$(grep '^sched:' /proc/kdebug)
sum "mem-zombie: $s"
[ "$(field "$s" space_frees_under_lock)" = 0 ] || { sum "mem-zombie: an address space was freed under SCHEDULER"; fail=1; }
sum "mem-zombie: $(grep '^ext2_cache:' /proc/kdebug)"
sum "mem-zombie: warnings on the log: $(grep -a -c 'WARNING: an address space' /proc/dmesg)"

echo "---- summary (the log wraps) ----"
cat /tmp/mem-zombie.sum
sum "mem-zombie: verdict exit=$fail"
exit $fail
