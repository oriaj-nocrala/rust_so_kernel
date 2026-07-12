#![no_std]
#![no_main]

use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use userspace::{eprintln, syscall};

static USR1_RECEIVED: AtomicBool = AtomicBool::new(false);
static USR1_SIGNUM: AtomicI32 = AtomicI32::new(-1);
static CHLD_RECEIVED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_usr1(sig: i32) {
    USR1_SIGNUM.store(sig, Ordering::SeqCst);
    USR1_RECEIVED.store(true, Ordering::SeqCst);
}

extern "C" fn on_chld(_sig: i32) {
    CHLD_RECEIVED.store(true, Ordering::SeqCst);
}

#[no_mangle]
extern "C" fn _start() -> ! {
    if syscall::sigaction(syscall::SIGUSR1, on_usr1 as usize as u64) < 0 {
        eprintln!("signal_test: sigaction(SIGUSR1) failed");
        syscall::exit(1);
    }
    if syscall::sigaction(syscall::SIGCHLD, on_chld as usize as u64) < 0 {
        eprintln!("signal_test: sigaction(SIGCHLD) failed");
        syscall::exit(1);
    }

    let parent_pid = syscall::getpid();

    let pid = syscall::fork();
    if pid == 0 {
        // Child: signal the parent, then exit (which queues SIGCHLD too).
        syscall::kill(parent_pid, syscall::SIGUSR1);
        syscall::exit(0);
    } else if pid < 0 {
        eprintln!("signal_test: fork() failed ({})", pid);
        syscall::exit(1);
    }

    // Parent: give the handler a chance to run (kill() wakes us if we're
    // blocked; if we're just Ready/Running, yield until it lands).
    let mut spins = 0;
    while !USR1_RECEIVED.load(Ordering::SeqCst) && spins < 10_000 {
        syscall::yield_now();
        spins += 1;
    }

    syscall::waitpid(pid);

    let mut spins2 = 0;
    while !CHLD_RECEIVED.load(Ordering::SeqCst) && spins2 < 10_000 {
        syscall::yield_now();
        spins2 += 1;
    }

    let usr1_ok = USR1_RECEIVED.load(Ordering::SeqCst)
        && USR1_SIGNUM.load(Ordering::SeqCst) == syscall::SIGUSR1 as i32;
    let chld_ok = CHLD_RECEIVED.load(Ordering::SeqCst);

    if usr1_ok && chld_ok {
        eprintln!("signal_test: PASS (SIGUSR1 delivered+resumed via sigreturn, SIGCHLD delivered)");
    } else {
        eprintln!("signal_test: FAIL (usr1_ok={}, chld_ok={})", usr1_ok, chld_ok);
    }

    syscall::exit(0);
}
