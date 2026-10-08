# Files on the Ryzen: where the time goes when a folder opens and when the selection moves (docs/gui/files-handoff.md step 1).
#   echo 1 > target/metal/budget
#   scripts/metal-run.sh --kconf 'gpu=uapi' scripts/metal-jobs/files-bench.sh
# In QEMU (no GPU) it runs under the CPU `compositor`; on the Ryzen under `vk_comp` when /dev/nvgpu is backed by the hardware.
# `files --bench DIR...` drives itself: per folder it opens it, waits for the preview, Down 50 times, PageDown 10 times, and prints one
# summary (`files: bench ...`: read_dir, the first frame after the change, per-frame medians/p95/max split into handle/render/wait/paint/
# semantics/present, stat calls, and each preview's spawn / exit seen / decode). The folders:
#   /mnt/bin    programs on the stick (ext2 over USB-MSC)
#   /mnt/many   2000 empty files on the stick: made once, kept
#   /tmp/many   the same 2000 in RAM (tmpfs): the difference with /mnt/many is the storage, what stays is the app
# Twice over /mnt/bin: the second run shows what the block cache saves. Passes if the bench reached its end.
fail=0
sum() { echo "$*"; echo "$*" >> /tmp/files-bench.sum; }
: > /tmp/files-bench.sum
dirs="/mnt/bin"
mk() { # mk <dir>: 2000 empty files, then the dir joins the bench (not when it cannot be written: /mnt over IDE in QEMU is read-only)
  t=$(cut -d' ' -f1 /proc/uptime)
  if [ ! -e "$1/f1999.txt" ]; then
    mkdir -p "$1" 2>/dev/null && touch "$1/f0.txt" 2>/dev/null || { sum "files-bench: cannot write $1, left out"; return; }
    i=1; while [ $i -lt 2000 ]; do touch "$1/f$i.txt"; i=$((i + 1)); done
  fi
  sum "files-bench: $1 ready ($t .. $(cut -d' ' -f1 /proc/uptime) s)"
  dirs="$dirs $1"
}
mk /mnt/many
mk /tmp/many
dirs="$dirs /mnt/bin"
if grep -q '^uapi: installed' /proc/gpu 2>/dev/null && [ -x /mnt/bin/vk_comp ]; then
  sum "files-bench: under vk_comp"
  COMP_NO_INPUT=1 COMP_EXIT_WHEN_IDLE=1 COMP_SECONDS=120 /mnt/bin/vk_comp "files --bench $dirs" > /tmp/fb.out 2>&1
  sum "files-bench: vk_comp exit=$?"
else
  sum "files-bench: under the CPU compositor"
  compositor "files --bench $dirs" > /tmp/fb.out 2>&1 &
  cpid=$!
  n=0
  while [ $n -lt 120 ] && ! grep -q 'files: bye\|files: cannot\|Killed PID [0-9]* (files)' /tmp/fb.out /proc/dmesg 2>/dev/null; do sleep 1; n=$((n + 1)); done
  kill $cpid 2>/dev/null; wait $cpid 2>/dev/null
fi
grep -a 'files: bench\|files: cannot\|files: preview .*failed' /tmp/fb.out | while read -r l; do sum "$l"; done
grep -aq 'files: bench done' /tmp/fb.out || { sum "files-bench: the bench did not finish"; tail -n 20 /tmp/fb.out >> /tmp/files-bench.sum; fail=1; }
echo "---- summary ----"
cat /tmp/files-bench.sum
sum "files-bench: verdict exit=$fail"
exit $fail
