// Userspace control for kernel/src/debug.rs's runtime-toggleable tracing
// subsystems — turns individual subsystems on/off live (no rebuild) via
// the custom kdebug_ctl syscall (403). See kernel::debug's doc comment for
// why this exists: tracepoints stay in the code permanently instead of
// being hand-added and stripped out per bug, gated so they're silent by
// default.
//
// Talks straight to the syscall instruction (no mlibc wrapper exists for
// this kernel-specific syscall) using the exact same rax=nr,
// rdi/rsi/rdx/r10/r8=args convention as mlibc-port/constanos-sysdeps's own
// internal raw_syscall() (kernel/src/process/syscall.rs's ABI doc comment).
#include <stdio.h>
#include <string.h>

#define SYS_KDEBUG_CTL 403
#define SYS_SYNC 162

static long raw_syscall(long nr, long a1, long a2, long a3) {
    long ret;
    register long r10 asm("r10") = 0;
    register long r8  asm("r8")  = 0;
    asm volatile ("syscall"
            : "=a"(ret)
            : "a"(nr), "D"(a1), "S"(a2), "d"(a3), "r"(r10), "r"(r8)
            : "rcx", "r11", "memory");
    return ret;
}

static void usage(void) {
    printf("usage: kdebug                    show current mask\n");
    printf("       kdebug <subsystem> <on|off>  (mm, sched, fs, proc)\n");
    printf("       kdebug sync               copy the kernel log to the USB stick\n");
    printf("       kdebug panic              panic the kernel on purpose (tests the panic path)\n");
}

// sync(2) — in this kernel, the flush of the log ring to the pendrive's
// `constanos-log` partition (kernel/src/block/logpart.rs). Unlike Linux's
// it reports failure, and says why, so the result is worth printing.
static int do_sync(void) {
    long r = raw_syscall(SYS_SYNC, 0, 0, 0);
    switch (r) {
    case 0:   printf("kdebug: log written to the USB stick\n"); return 0;
    case -19: printf("kdebug: no log partition in use (see /proc/dmesg: klog-disk)\n"); return 1;
    case -16: printf("kdebug: a flush is already running\n"); return 1;
    default:  printf("kdebug: log flush failed (%ld)\n", r); return 1;
    }
}

int main(int argc, char **argv) {
    if (argc == 1) {
        long mask = raw_syscall(SYS_KDEBUG_CTL, 0, 0, 0);
        printf("kdebug: mask=0x%lx\n", mask);
        usage();
        return 0;
    }

    if (argc == 2 && strcmp(argv[1], "sync") == 0) {
        return do_sync();
    }
    if (argc == 2 && strcmp(argv[1], "panic") == 0) {
        raw_syscall(SYS_KDEBUG_CTL, 2, 0, 0);
        printf("kdebug: still alive?\n");
        return 1;
    }

    if (argc != 3) {
        usage();
        return 1;
    }

    int enable;
    if (strcmp(argv[2], "on") == 0) {
        enable = 1;
    } else if (strcmp(argv[2], "off") == 0) {
        enable = 0;
    } else {
        usage();
        return 1;
    }

    long r = raw_syscall(SYS_KDEBUG_CTL, 1, (long)argv[1], enable);
    if (r < 0) {
        printf("kdebug: unknown subsystem '%s'\n", argv[1]);
        return 1;
    }
    printf("kdebug: %s %s -> mask=0x%lx\n", argv[1], enable ? "on" : "off", r);
    return 0;
}
