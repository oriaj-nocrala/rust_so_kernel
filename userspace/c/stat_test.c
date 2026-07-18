// Smoke test for the mlibc stat()/fstat()/waitpid()/opendir() sysdeps wiring (generic.cpp).
#include <stdio.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <fcntl.h>
#include <unistd.h>
#include <dirent.h>
#include <poll.h>

int main(void) {
    struct stat st;

    if (stat("/dev/null", &st) != 0) {
        printf("stat(/dev/null) FAILED\n");
        return 1;
    }
    printf("stat(/dev/null): mode=0x%x size=%ld\n", st.st_mode, (long)st.st_size);

    int fd = open("/dev/null", O_RDONLY);
    if (fd < 0) {
        printf("open(/dev/null) FAILED\n");
        return 1;
    }

    struct stat fst;
    if (fstat(fd, &fst) != 0) {
        printf("fstat(/dev/null) FAILED\n");
        return 1;
    }
    printf("fstat(/dev/null): mode=0x%x nlink=%lu\n", fst.st_mode, (unsigned long)fst.st_nlink);

    close(fd);
    printf("stat_test: OK\n");

    pid_t child = fork();
    if (child < 0) {
        printf("fork FAILED\n");
        return 1;
    }
    if (child == 0) {
        _exit(0);
    }

    int status = -1;
    pid_t reaped = waitpid(child, &status, 0);
    if (reaped != child) {
        printf("waitpid FAILED: reaped=%d child=%d\n", (int)reaped, (int)child);
        return 1;
    }
    printf("waitpid: reaped=%d WIFEXITED=%d WEXITSTATUS=%d\n",
           (int)reaped, WIFEXITED(status), WEXITSTATUS(status));
    printf("waitpid_test: OK\n");

    DIR *dir = opendir("/dev");
    if (!dir) {
        printf("opendir(/dev) FAILED\n");
        return 1;
    }
    int count = 0;
    struct dirent *ent;
    while ((ent = readdir(dir)) != NULL) {
        printf("  dirent: %s (type=%d)\n", ent->d_name, ent->d_type);
        count++;
    }
    closedir(dir);
    printf("opendir_test: OK (%d entries)\n", count);

    int pfd[2];
    if (pipe(pfd) != 0) {
        printf("pipe FAILED\n");
        return 1;
    }
    write(pfd[1], "x", 1);

    struct pollfd pfds[1];
    pfds[0].fd = pfd[0];
    pfds[0].events = POLLIN;
    pfds[0].revents = 0;

    int n = poll(pfds, 1, 1000);
    if (n != 1 || !(pfds[0].revents & POLLIN)) {
        printf("poll FAILED: n=%d revents=0x%x\n", n, pfds[0].revents);
        return 1;
    }
    printf("poll: n=%d revents=0x%x\n", n, pfds[0].revents);
    close(pfd[0]);
    close(pfd[1]);
    printf("poll_test: OK\n");
    return 0;
}
