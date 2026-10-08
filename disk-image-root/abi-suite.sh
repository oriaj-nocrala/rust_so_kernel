#!/bin/sh
# Runs the C test programs one after another inside the guest and prints `SUITE_RESULT <name>=<exit code>` for each, then
# SUITE_DONE. With arguments, runs just those. Driven by scripts/run-abi-suite.sh (one boot, one typed command).
tests="$*"
if [ -z "$tests" ]; then
    tests="linux_abi_test mprotect_test cloexec_test sigabi_test sigsegv_test pipe_poll_test fdlimit_test exitgroup_test tgid_test
           siginfo_test nanosleep_rem_test epoll_scale_test vfork_test eventfd_test unix_nb_test pidfd_test maps_test at_test openat2_test nx_test cap_rights_test capmode_test cap_exec_test pio_test fdlock_test timer_test lifecycle_test jobctl_test pthread_test sigsuspend_test
           pipe_multi_test fork_exec_test socket_test pty_test wait_intr_test session_test mlibc_signal_test cputime_test
           fstime_test input_poll_test wcontinued_test zombie_mem_test creds_test statx_test link_test umask_test shebang_test vmshare_test seqpacket_test poll_file_test nvgpu_sw_test nvgpu_hw_test"
fi
# Per test: its time (SUITE_TIME, /proc/uptime before and after) and, when it failed or was slow, its syscall profile
# (SUITE_PROF, /proc/sysprof: calls, wall time and the slowest call per syscall, per program). One watcher for the whole run: a test still running 15-20 s after
# it started gets its live profile dumped once (SUITE_HANG), with the call each process is in right now, before the host
# gives up on it. (One watcher, not one per test: killing a per-test one logged a death per test.)
kdebug sysprof on >/dev/null 2>&1
: > /tmp/suite.cur
(
    last=; n=0
    while sleep 5; do
        read c < /tmp/suite.cur
        if [ -n "$c" ] && [ "$c" = "$last" ]; then n=$((n + 5)); else n=0; fi
        [ "$n" = 15 ] && sed "s/^/SUITE_HANG ${c%% *} /" /proc/sysprof
        last=$c
    done
) &
watch=$!
seq=0
for t in $tests; do
    echo "SUITE_START $t"
    seq=$((seq + 1))
    echo "$t $seq" > /tmp/suite.cur
    kdebug sysprof reset >/dev/null 2>&1
    t0=$(cut -d' ' -f1 /proc/uptime)
    /mnt/bin/$t
    rc=$?
    t1=$(cut -d' ' -f1 /proc/uptime)
    : > /tmp/suite.cur
    echo "SUITE_RESULT $t=$rc"
    echo "SUITE_TIME $t $t0 $t1"
    # The profile of a failed or slow (> 2 s) test only: the others' would be a third of serial.log for nobody.
    # /proc/uptime has two decimals, so "12.34" -> 1234 centiseconds.
    cs=$(( ${t1%.*}${t1#*.} - ${t0%.*}${t0#*.} ))
    if [ "$rc" != 0 ] || [ "$cs" -gt 200 ]; then sed "s/^/SUITE_PROF $t /" /proc/sysprof; fi
done
kill $watch 2>/dev/null
kdebug sysprof off >/dev/null 2>&1
echo SUITE_DONE
