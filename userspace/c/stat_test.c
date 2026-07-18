// Smoke test for the mlibc stat()/fstat()/waitpid()/opendir() sysdeps wiring (generic.cpp).
#include <stdio.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <fcntl.h>
#include <unistd.h>
#include <dirent.h>
#include <poll.h>
#include <errno.h>

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

    if (mkdir("/tmp/mkdir_test", 0755) != 0) {
        printf("mkdir FAILED\n");
        return 1;
    }
    if (mkdir("/tmp/mkdir_test", 0755) == 0 || errno != EEXIST) {
        printf("mkdir(dup) should have failed EEXIST, errno=%d\n", errno);
        return 1;
    }

    int afd = open("/tmp/mkdir_test/a.txt", O_CREAT | O_WRONLY, 0644);
    if (afd < 0) {
        printf("open(a.txt, O_CREAT) FAILED\n");
        return 1;
    }
    write(afd, "hi", 2);
    close(afd);

    if (mkdir("/tmp/mkdir_test/sub", 0755) != 0) {
        printf("mkdir(nested) FAILED\n");
        return 1;
    }

    DIR *td = opendir("/tmp/mkdir_test");
    if (!td) {
        printf("opendir(mkdir_test) FAILED\n");
        return 1;
    }
    int tcount = 0;
    struct dirent *tent;
    while ((tent = readdir(td)) != NULL) {
        printf("  mkdir_test/: %s (type=%d)\n", tent->d_name, tent->d_type);
        tcount++;
    }
    closedir(td);
    if (tcount != 4) { // . .. a.txt sub
        printf("mkdir_test dir listing FAILED: count=%d\n", tcount);
        return 1;
    }

    if (rmdir("/tmp/mkdir_test") == 0 || errno != ENOTEMPTY) {
        printf("rmdir(non-empty) should have failed ENOTEMPTY, errno=%d\n", errno);
        return 1;
    }

    if (rename("/tmp/mkdir_test/a.txt", "/tmp/mkdir_test/b.txt") != 0) {
        printf("rename FAILED\n");
        return 1;
    }
    struct stat rst;
    if (stat("/tmp/mkdir_test/a.txt", &rst) == 0 || stat("/tmp/mkdir_test/b.txt", &rst) != 0) {
        printf("rename didn't actually move the file\n");
        return 1;
    }

    if (unlink("/tmp/mkdir_test/b.txt") != 0) {
        printf("unlink FAILED\n");
        return 1;
    }
    if (rmdir("/tmp/mkdir_test/sub") != 0) {
        printf("rmdir(sub) FAILED\n");
        return 1;
    }
    if (rmdir("/tmp/mkdir_test") != 0) {
        printf("rmdir(mkdir_test, now empty) FAILED\n");
        return 1;
    }
    if (stat("/tmp/mkdir_test", &rst) == 0) {
        printf("mkdir_test still exists after rmdir!\n");
        return 1;
    }
    printf("mkdir_test: OK\n");
    return 0;
}
