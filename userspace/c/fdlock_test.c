// Lock order between a process's fd table and the scheduler: threads share one table, sys_read/sys_write hold the table lock across
// FileHandle::read/write (which take the scheduler), so no syscall may take the table while holding the scheduler. One thread
// hammers fcntl / dup / close / fstat / open on descriptors while others read and write a pipe: the old order deadlocked every CPU
// within a few thousand iterations. A hang here is the failure.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <fcntl.h>
#include <pthread.h>
#include <sys/stat.h>

#define printf(...) ((printf)(__VA_ARGS__), fflush(stdout))
static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

#define ITER 20000
static int pfd[2];
static volatile long reads, writes, ctl;

static void *reader(void *arg) {
    (void)arg;
    char b[64];
    for (int i = 0; i < ITER; i++) { if (read(pfd[0], b, sizeof b) > 0) reads++; }
    return NULL;
}
static void *writer(void *arg) {
    (void)arg;
    for (int i = 0; i < ITER; i++) { if (write(pfd[1], "xxxxxxxx", 8) > 0) writes++; }
    return NULL;
}
static void *fiddler(void *arg) {
    (void)arg;
    struct stat st;
    for (int i = 0; i < ITER; i++) {
        fcntl(pfd[0], F_SETFL, O_NONBLOCK);
        fcntl(pfd[1], F_GETFL);
        int d = dup(pfd[0]);
        fcntl(d, F_SETFD, FD_CLOEXEC);
        fstat(d, &st);
        close(d);
        int f = open("/proc/uptime", O_RDONLY);
        if (f >= 0) close(f);
        ctl++;
    }
    return NULL;
}

int main(void) {
    printf("threads hammer the shared fd table while others read and write a pipe\n");
    pipe(pfd);
    fcntl(pfd[0], F_SETFL, O_NONBLOCK);
    fcntl(pfd[1], F_SETFL, O_NONBLOCK);
    pthread_t t[4];
    pthread_create(&t[0], NULL, reader, NULL);
    pthread_create(&t[1], NULL, writer, NULL);
    pthread_create(&t[2], NULL, fiddler, NULL);
    pthread_create(&t[3], NULL, fiddler, NULL);
    for (int i = 0; i < 4; i++) pthread_join(t[i], NULL);
    CHECK(ctl == 2 * ITER, "the fiddlers finished %ld of %d iterations", ctl, 2 * ITER);
    CHECK(writes > 0 && reads > 0, "writes %ld reads %ld", writes, reads);
    printf(failures ? "fdlock_test: FAIL\n" : "fdlock_test: PASS\n");
    return failures != 0;
}
