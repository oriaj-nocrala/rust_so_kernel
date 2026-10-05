// A fresh anonymous mapping read before it is written: the read maps the shared zero frame read-only, the write must then get a
// private writable page. The kernel once created the page-table levels of that first (read-only) mapping read-only too, so every
// later write in the same 2 MiB faulted with the PTE already writable and the process looped on the fault forever (the CPU
// compositor drawing a title into a zeroed buffer). Raw `syscall` on purpose: this checks the kernel. The work runs in a child with
// an alarm, so a loop shows as a SIGALRM death instead of a hang. See docs/reference/memory.md.
#include <stdio.h>
#include <signal.h>
#include <unistd.h>
#include <sys/wait.h>

static long sc(long nr, long a, long b, long c, long d, long e, long f) {
    long ret;
    register long r10 __asm__("r10") = d;
    register long r8 __asm__("r8") = e;
    register long r9 __asm__("r9") = f;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8), "r"(r9) : "rcx", "r11", "memory");
    return ret;
}

enum { SYS_mmap = 9, SYS_alarm = 37 };
#define PG 4096UL
#define LEN (1UL << 20)   // several 1 MiB mappings: some start in a 2 MiB region no page table covers yet
#define N 6

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

int main(void) {
    printf("zeropage_test: read, then write, fresh anonymous memory\n");
    pid_t pid = fork();
    if (pid == 0) {
        sc(SYS_alarm, 5, 0, 0, 0, 0, 0);
        volatile unsigned char *m[N];
        for (int i = 0; i < N; i++) {
            long r = sc(SYS_mmap, 0, LEN, 3, 0x22, -1, 0);
            if (r < 0 && r > -4096) _exit(2);
            m[i] = (volatile unsigned char *)r;
        }
        unsigned sum = 0;
        for (int i = 0; i < N; i++)
            for (unsigned long o = 0; o < LEN; o += PG) sum += m[i][o];   // every page read first: the zero frame
        for (int i = 0; i < N; i++)
            for (unsigned long o = 0; o < LEN; o += PG) m[i][o + 7] = (unsigned char)(o >> 12);   // then written
        for (int i = 0; i < N; i++)
            for (unsigned long o = 0; o < LEN; o += PG)
                if (m[i][o + 7] != (unsigned char)(o >> 12) || m[i][o] != 0) _exit(3);
        _exit(sum == 0 ? 0 : 4);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "child %s %d (SIGALRM = 14: it looped on a write fault)",
          WIFSIGNALED(st) ? "killed by signal" : "exited with", WIFSIGNALED(st) ? WTERMSIG(st) : WEXITSTATUS(st));
    printf(failures ? "zeropage_test: FAILED (%d)\n" : "zeropage_test: DONE\n", failures);
    return failures ? 1 : 0;
}
