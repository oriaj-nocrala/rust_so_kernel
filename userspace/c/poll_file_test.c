// poll() on fd 0 after a regular file was redirected onto it (`read l < file` in ash polls fd 0 with an infinite timeout
// before reading): the file is always ready. fd 0 used to be treated as the keyboard whatever it was, so the poll slept
// until a key was pressed. While fd 0 is still the console, an empty keyboard buffer is "not ready".
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <poll.h>
#include <errno.h>
#include <time.h>

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

static long ms_since(const struct timespec *t0) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (t.tv_sec - t0->tv_sec) * 1000 + (t.tv_nsec - t0->tv_nsec) / 1000000;
}

static void file_on_fd0(const char *path) {
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    CHECK(fd >= 0, "open %s (errno %d)", path, errno);
    if (fd < 0) return;
    write(fd, "a b\nc\n", 6);
    lseek(fd, 0, SEEK_SET);
    int saved = dup(0);
    CHECK(dup2(fd, 0) == 0, "dup2 onto 0");
    close(fd);

    struct pollfd p = { .fd = 0, .events = POLLIN };
    struct timespec t0;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    int r = poll(&p, 1, 3000);                       // ash's `read` waits with -1; 3 s keeps a broken kernel from hanging the suite
    long ms = ms_since(&t0);
    CHECK(r == 1 && (p.revents & POLLIN), "%s on fd 0: poll returned %d revents %#x", path, r, p.revents);
    CHECK(ms < 1000, "%s on fd 0: poll took %ld ms (waited for a key?)", path, ms);

    struct pollfd w = { .fd = 0, .events = POLLOUT };
    CHECK(poll(&w, 1, 0) == 1 && (w.revents & POLLOUT), "%s on fd 0: POLLOUT", path);

    char c[8] = {0};
    CHECK(read(0, c, 3) == 3 && !memcmp(c, "a b", 3), "read(0) reads the file, not the keyboard");

    dup2(saved, 0);
    close(saved);
    unlink(path);
}

int main(void) {
    printf("poll_file_test:\n");
    if (isatty(0)) {
        struct pollfd k = { .fd = 0, .events = POLLIN };
        CHECK(poll(&k, 1, 0) == 0, "fd 0 is the console and no key was pressed: not ready (revents %#x)", k.revents);
    }
    file_on_fd0("/tmp/poll_file_test");
    file_on_fd0("/mnt/poll_file_test");
    if (failures) { printf("poll_file_test: %d FAILED\n", failures); return 1; }
    printf("poll_file_test: OK\n");
    return 0;
}
