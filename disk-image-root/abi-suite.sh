#!/bin/sh
# Runs the C test programs one after another inside the guest and prints `SUITE_RESULT <name>=<exit code>` for each, then
# SUITE_DONE. With arguments, runs just those. Driven by scripts/run-abi-suite.sh (one boot, one typed command).
tests="$*"
if [ -z "$tests" ]; then
    tests="linux_abi_test mprotect_test cloexec_test sigabi_test sigsegv_test pipe_poll_test fdlimit_test exitgroup_test tgid_test
           siginfo_test nanosleep_rem_test epoll_scale_test vfork_test eventfd_test unix_nb_test pidfd_test maps_test at_test fdlock_test timer_test lifecycle_test jobctl_test pthread_test sigsuspend_test
           pipe_multi_test fork_exec_test socket_test pty_test wait_intr_test session_test mlibc_signal_test cputime_test
           fstime_test input_poll_test wcontinued_test creds_test statx_test link_test"
fi
for t in $tests; do
    echo "SUITE_START $t"
    /mnt/bin/$t
    echo "SUITE_RESULT $t=$?"
done
echo SUITE_DONE
