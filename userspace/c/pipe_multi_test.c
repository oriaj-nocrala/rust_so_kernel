// pipe_multi_test: several processes blocked on the same end of one pipe.
//
// A pipe used to keep a single waiting reader and a single waiting writer:
// a second process blocking on the same end replaced the first, which then
// never woke (found by shm_test's first case 9 — four children blocked on
// one pipe as a barrier, one woke). Every case forks NPROC children that
// block on one pipe, then does the thing that must release all of them.
//
//   A. readers, write end closed: every reader gets EOF;
//   B. readers, one byte each: NPROC bytes written at once reach NPROC
//      different readers, each exactly once;
//   C. writers, pipe full: reading drains every blocked writer, and no byte
//      is lost or duplicated (per-writer counts are exact);
//   D. writers, read end closed: every writer gets EPIPE.
//
// A child that never wakes cannot be killed either (signals do not wake a
// blocked process here), so each case waits with a deadline and reports
// FAIL instead of hanging.
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/wait.h>

#define NPROC 4
#define PIPE_CAP 4096
#define PER_WRITER 3000
static int fails;

static void check(int ok, const char *what) {
    printf("%s: %s\n", what, ok ? "PASS" : "FAIL");
    if (!ok) fails++;
}

static void nap_ms(long ms) {
    struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L };
    nanosleep(&ts, NULL);
}

// Reap `n` children within `ms`; each one's exit code lands in `codes`
// (in reaping order). Returns how many were reaped.
static int reap_all(int n, int *codes, long ms) {
    int got = 0;
    for (long waited = 0; got < n && waited <= ms; ) {
        int st;
        pid_t p = waitpid(-1, &st, WNOHANG);
        if (p > 0) {
            codes[got++] = WIFEXITED(st) ? WEXITSTATUS(st) : 255;
        } else {
            nap_ms(10);
            waited += 10;
        }
    }
    return got;
}

static void case_a(void) {
    int p[2];
    pipe(p);
    for (int i = 0; i < NPROC; i++) {
        if (fork() == 0) {
            close(p[1]);
            char c;
            _exit(read(p[0], &c, 1) == 0 ? 0 : 1);
        }
    }
    close(p[0]);
    nap_ms(200); // let them all block
    close(p[1]);
    int codes[NPROC], n = reap_all(NPROC, codes, 3000), ok = n == NPROC;
    for (int i = 0; i < n; i++) ok &= codes[i] == 0;
    printf("  A reaped %d/%d\n", n, NPROC);
    check(ok, "A blocked readers all get EOF");
}

static void case_b(void) {
    int p[2];
    pipe(p);
    for (int i = 0; i < NPROC; i++) {
        if (fork() == 0) {
            close(p[1]);
            unsigned char c;
            _exit(read(p[0], &c, 1) == 1 ? c : 0);
        }
    }
    close(p[0]);
    nap_ms(200);
    write(p[1], "abcd", NPROC);
    int codes[NPROC], n = reap_all(NPROC, codes, 3000);
    int seen[NPROC] = {0}, ok = n == NPROC;
    for (int i = 0; i < n; i++) {
        int k = codes[i] - 'a';
        if (k < 0 || k >= NPROC || seen[k]++) ok = 0;
    }
    close(p[1]);
    if (n < NPROC) reap_all(NPROC - n, codes, 1000); // released by the EOF
    printf("  B reaped %d/%d\n", n, NPROC);
    check(ok, "B one byte each: every reader gets a distinct byte");
}

static void case_c(void) {
    int p[2];
    pipe(p);
    char fill[PIPE_CAP];
    memset(fill, '.', sizeof fill);
    write(p[1], fill, sizeof fill); // full: every child write blocks
    for (int i = 0; i < NPROC; i++) {
        if (fork() == 0) {
            close(p[0]);
            char buf[PER_WRITER];
            memset(buf, 'A' + i, sizeof buf);
            size_t done = 0;
            while (done < sizeof buf) {
                ssize_t w = write(p[1], buf + done, sizeof buf - done);
                if (w <= 0) _exit(1);
                done += w;
            }
            _exit(0);
        }
    }
    close(p[1]);
    nap_ms(200);
    long count[NPROC + 1] = {0}; // [NPROC] counts the '.' fill
    char buf[512];
    ssize_t r;
    long total = 0;
    while ((r = read(p[0], buf, sizeof buf)) > 0) {
        for (ssize_t j = 0; j < r; j++) {
            int k = buf[j] == '.' ? NPROC : buf[j] - 'A';
            if (k >= 0 && k <= NPROC) count[k]++;
        }
        total += r;
    }
    close(p[0]);
    int codes[NPROC], n = reap_all(NPROC, codes, 3000), ok = n == NPROC;
    for (int i = 0; i < n; i++) ok &= codes[i] == 0;
    ok &= count[NPROC] == PIPE_CAP;
    for (int i = 0; i < NPROC; i++) ok &= count[i] == PER_WRITER;
    printf("  C reaped %d/%d, read %ld bytes (A=%ld B=%ld C=%ld D=%ld fill=%ld)\n",
           n, NPROC, total, count[0], count[1], count[2], count[3], count[NPROC]);
    check(ok, "C blocked writers all drain, no byte lost or duplicated");
}

static void case_d(void) {
    int p[2];
    pipe(p);
    char fill[PIPE_CAP];
    memset(fill, '.', sizeof fill);
    write(p[1], fill, sizeof fill);
    for (int i = 0; i < NPROC; i++) {
        if (fork() == 0) {
            signal(SIGPIPE, SIG_IGN);
            close(p[0]);
            char c = 'x';
            _exit(write(p[1], &c, 1) == -1 && errno == EPIPE ? 0 : 1);
        }
    }
    close(p[1]);
    nap_ms(200);
    close(p[0]);
    int codes[NPROC], n = reap_all(NPROC, codes, 3000), ok = n == NPROC;
    for (int i = 0; i < n; i++) ok &= codes[i] == 0;
    printf("  D reaped %d/%d\n", n, NPROC);
    check(ok, "D blocked writers all get EPIPE");
}

int main(void) {
    case_a();
    case_b();
    case_c();
    case_d();
    printf("pipe_multi_test: %s (%d failed)\n", fails ? "FAIL" : "PASS", fails);
    return fails ? 1 : 0;
}
