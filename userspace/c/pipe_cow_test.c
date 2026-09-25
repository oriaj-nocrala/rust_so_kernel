// pipe_cow_test: a blocked pipe reader's buffer is filled by the *writer*
// (kernel/src/process/pipe.rs completes the read from the waker's context,
// translating through the reader's address space). That copy must behave
// like a user-mode write into the reader's page:
//
//   A. a page the reader only ever *read* maps the shared zero frame; the
//      copy must not land in it (every untouched page in the system would
//      then read the pipe's bytes instead of zeros);
//   B. a page still COW-shared with a fork sibling must be un-shared first
//      (otherwise the sibling's copy of the page changes under it).
//
// The writer sleeps first so the reader is really blocked when it writes.
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <time.h>
#include <sys/wait.h>
#include <sys/mman.h>

#define PAGE 4096

static char cowbuf[4 * PAGE]; // written before fork: COW-shared after

static void nap_ms(long ms) {
    struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L };
    nanosleep(&ts, NULL);
}

static int case_zero_frame(void) {
    int p[2];
    if (pipe(p) != 0) { printf("A: pipe failed\n"); return 1; }
    // Anonymous memory: demand-paged, so a read fault maps the zero frame
    // (bss is not — the ELF loader maps it eagerly).
    char *zbuf = mmap(NULL, 4 * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *probe = mmap(NULL, 4 * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (zbuf == MAP_FAILED || probe == MAP_FAILED) { printf("A: mmap failed\n"); return 1; }
    char *dst = &zbuf[2 * PAGE];
    volatile char touch = *dst; // read fault -> zero frame, read-only
    (void)touch;

    pid_t pid = fork();
    if (pid == 0) {
        close(p[0]);
        nap_ms(300);
        write(p[1], "HELLO", 5);
        _exit(0);
    }
    close(p[1]);
    ssize_t n = read(p[0], dst, 5);
    waitpid(pid, NULL, 0);
    close(p[0]);

    char seen = probe[2 * PAGE]; // read fault -> zero frame
    int ok = n == 5 && memcmp(dst, "HELLO", 5) == 0 && seen == 0;
    printf("A zero-frame: read=%ld got='%.5s' untouched page byte=%d -> %s\n",
           (long)n, dst, seen, ok ? "PASS" : "FAIL");
    return !ok;
}

static int case_cow_sibling(void) {
    int p[2];
    if (pipe(p) != 0) { printf("B: pipe failed\n"); return 1; }
    char *buf = &cowbuf[2 * PAGE];
    memset(buf, 'A', 8);

    pid_t pid = fork();
    if (pid == 0) {
        // Reader: blocks with its buffer still shared with the parent.
        close(p[1]);
        ssize_t n = read(p[0], buf, 8);
        _exit(n == 8 && memcmp(buf, "BBBBBBBB", 8) == 0 ? 0 : 1);
    }
    close(p[0]);
    nap_ms(300);
    write(p[1], "BBBBBBBB", 8);
    int status = 0;
    waitpid(pid, &status, 0);
    close(p[1]);

    int child_ok = WIFEXITED(status) && WEXITSTATUS(status) == 0;
    int parent_ok = memcmp(buf, "AAAAAAAA", 8) == 0;
    printf("B cow-sibling: child got data=%s parent page='%.8s' -> %s\n",
           child_ok ? "yes" : "no", buf, child_ok && parent_ok ? "PASS" : "FAIL");
    return !(child_ok && parent_ok);
}

int main(void) {
    int fails = case_zero_frame() + case_cow_sibling();
    printf("pipe_cow_test: %s\n", fails ? "FAIL" : "PASS");
    return fails ? 1 : 0;
}
