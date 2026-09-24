// fork_exec_test: does a child's fork + exec + exit leave the parent's
// memory alone?
//
// Written for the bug the first bare-metal autorun job found (2026-09-24):
// BusyBox `ash` running a script died at its `exit` builtin — a longjmp to
// rip 0, 0x246 (an RFLAGS value) or an address inside the *child's* binary —
// but only after running a small external program (`uname`, `uptime`,
// `kdebug`), never `hello` or `busybox true`. Those values look like the
// child's stack showing through the parent's, so this fills the parent's
// stack, heap and a global with a pattern, runs children, and checks every
// byte after each one — no ash involved.
//
// usage: fork_exec_test [PROGRAM [ROUNDS [sig]]]   (default /bin/uname, 20)
// `sig` also installs a SIGCHLD handler and longjmps once per round, the two
// things ash does that a plain fork/exec/wait does not.
// Exit status 0 = every region intact after every round.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>
#include <signal.h>
#include <setjmp.h>

#define REGION (16 * 1024)

static unsigned char global_buf[REGION];
static volatile sig_atomic_t got_sigchld;
static jmp_buf jb;

static void on_sigchld(int sig) {
    (void)sig;
    got_sigchld++;
}

static void fill(volatile unsigned char *p, size_t n, unsigned seed) {
    for (size_t i = 0; i < n; i++)
        p[i] = (unsigned char)(seed + i * 7);
}

// Returns the first corrupted offset, or -1.
static long check(volatile unsigned char *p, size_t n, unsigned seed) {
    for (size_t i = 0; i < n; i++)
        if (p[i] != (unsigned char)(seed + i * 7))
            return (long)i;
    return -1;
}

static int report(const char *what, volatile unsigned char *p, long off, int round) {
    if (off < 0)
        return 0;
    printf("fork_exec_test: round %d: %s corrupted at offset %ld (%p):",
           round, what, off, (void *)(p + off));
    for (long i = off; i < off + 16 && i < REGION; i++)
        printf(" %02x", p[i]);
    printf("\n");
    return 1;
}

int main(int argc, char **argv) {
    const char *prog = argc > 1 ? argv[1] : "/bin/uname";
    int rounds = argc > 2 ? atoi(argv[2]) : 20;
    int with_sig = argc > 3 && strcmp(argv[3], "sig") == 0;

    if (with_sig) {
        struct sigaction sa;
        memset(&sa, 0, sizeof sa);
        sa.sa_handler = on_sigchld;
        sigaction(SIGCHLD, &sa, NULL);
    }

    volatile unsigned char stack_buf[REGION];
    unsigned char *heap_buf = malloc(REGION);
    if (!heap_buf) {
        printf("fork_exec_test: malloc failed\n");
        return 2;
    }

    fill(stack_buf, REGION, 0x11);
    fill(heap_buf, REGION, 0x55);
    fill(global_buf, REGION, 0x99);

    int bad = 0;
    for (int r = 0; r < rounds && !bad; r++) {
        pid_t pid = fork();
        if (pid == 0) {
            execl(prog, prog, (char *)NULL);
            _exit(127);
        }
        if (pid < 0) {
            printf("fork_exec_test: fork failed\n");
            return 2;
        }
        int st = 0;
        waitpid(pid, &st, 0);
        bad |= report("stack", stack_buf, check(stack_buf, REGION, 0x11), r);
        bad |= report("heap", heap_buf, check(heap_buf, REGION, 0x55), r);
        bad |= report("global", global_buf, check(global_buf, REGION, 0x99), r);
        if (with_sig) {
            volatile int jumped = 0;
            if (setjmp(jb) == 0) {
                jumped = 1;
                longjmp(jb, 1);
            }
            if (!jumped) {
                printf("fork_exec_test: round %d: longjmp came back wrong\n", r);
                bad = 1;
            }
        }
    }
    if (with_sig)
        printf("fork_exec_test: %d SIGCHLD delivered\n", (int)got_sigchld);

    printf("fork_exec_test: %s (%s, %d rounds%s)\n", bad ? "FAIL" : "PASS", prog, rounds,
           with_sig ? ", SIGCHLD handler" : "");
    return bad;
}
