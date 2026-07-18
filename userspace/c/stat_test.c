// Smoke test for the mlibc stat()/fstat()/waitpid()/opendir() sysdeps wiring (generic.cpp).
#include <stdio.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <fcntl.h>
#include <unistd.h>
#include <dirent.h>
#include <poll.h>
#include <errno.h>
#include <string.h>
#include <signal.h>

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

    // Real exit code, not 0 — proves the kernel is actually reporting the
    // child's own status instead of a hardcoded "exited(0)".
    pid_t child = fork();
    if (child < 0) {
        printf("fork FAILED\n");
        return 1;
    }
    if (child == 0) {
        _exit(42);
    }

    int status = -1;
    pid_t reaped = waitpid(child, &status, 0);
    if (reaped != child) {
        printf("waitpid FAILED: reaped=%d child=%d\n", (int)reaped, (int)child);
        return 1;
    }
    printf("waitpid: reaped=%d WIFEXITED=%d WEXITSTATUS=%d\n",
           (int)reaped, WIFEXITED(status), WEXITSTATUS(status));
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 42) {
        printf("waitpid_test FAILED: expected WIFEXITED && WEXITSTATUS==42\n");
        return 1;
    }

    // A child killed by an uncaught signal should report WIFSIGNALED/
    // WTERMSIG, not WIFEXITED.
    pid_t child2 = fork();
    if (child2 < 0) {
        printf("fork(2) FAILED\n");
        return 1;
    }
    if (child2 == 0) {
        kill(getpid(), SIGKILL);
        _exit(1); // unreachable if SIGKILL actually took effect
    }
    int status2 = -1;
    pid_t reaped2 = waitpid(child2, &status2, 0);
    if (reaped2 != child2) {
        printf("waitpid(2) FAILED: reaped=%d child=%d\n", (int)reaped2, (int)child2);
        return 1;
    }
    printf("waitpid(2): WIFSIGNALED=%d WTERMSIG=%d\n", WIFSIGNALED(status2), WTERMSIG(status2));
    if (!WIFSIGNALED(status2) || WTERMSIG(status2) != SIGKILL) {
        printf("waitpid_test(2) FAILED: expected WIFSIGNALED && WTERMSIG==SIGKILL\n");
        return 1;
    }
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

    // dup/dup2/fcntl: write a known 10-byte file, then prove a dup'd fd
    // shares the same *offset* (not just the same underlying file) —
    // that's the part that's easy to get wrong (a naive dup could hand
    // back an independent fd starting at 0 again).
    int wfd = open("/tmp/dup_test.txt", O_CREAT | O_TRUNC | O_WRONLY, 0644);
    if (wfd < 0) {
        printf("open(dup_test.txt, O_CREAT) FAILED\n");
        return 1;
    }
    write(wfd, "0123456789", 10);
    close(wfd);

    int rfd = open("/tmp/dup_test.txt", O_RDONLY);
    if (rfd < 0) {
        printf("open(dup_test.txt, O_RDONLY) FAILED\n");
        return 1;
    }
    int dupfd = dup(rfd);
    if (dupfd < 0) {
        printf("dup FAILED\n");
        return 1;
    }

    char half1[6] = {0}, half2[6] = {0};
    if (read(rfd, half1, 5) != 5) {
        printf("read via rfd FAILED\n");
        return 1;
    }
    if (read(dupfd, half2, 5) != 5) {
        printf("read via dupfd FAILED\n");
        return 1;
    }
    if (memcmp(half1, "01234", 5) != 0 || memcmp(half2, "56789", 5) != 0) {
        printf("dup offset NOT shared: half1=%s half2=%s\n", half1, half2);
        return 1;
    }
    printf("dup: shared offset OK (half1=%s half2=%s)\n", half1, half2);

    // dup2 onto a specific target: both ends still at EOF (10/10 read).
    if (dup2(rfd, 9) != 9) {
        printf("dup2 FAILED\n");
        return 1;
    }
    char tail[2] = {0};
    long n2 = read(9, tail, 1);
    if (n2 != 0) {
        printf("dup2 offset NOT shared: expected EOF, got n=%ld\n", n2);
        return 1;
    }
    close(9);
    close(dupfd);

    // fcntl(F_DUPFD, 5): same handle, forced to land at fd >= 5.
    int fdupfd = fcntl(rfd, F_DUPFD, 5);
    if (fdupfd < 5) {
        printf("fcntl(F_DUPFD) FAILED: got %d\n", fdupfd);
        return 1;
    }
    close(fdupfd);
    close(rfd);
    printf("dup_test: OK\n");

    // chdir/getcwd: real relative-path resolution, not just string storage.
    char cwdbuf[64];
    if (getcwd(cwdbuf, sizeof(cwdbuf)) == NULL || strcmp(cwdbuf, "/") != 0) {
        printf("getcwd(initial) FAILED: got '%s'\n", cwdbuf);
        return 1;
    }

    if (chdir("/tmp/mkdir_test_does_not_exist") == 0 || errno != ENOENT) {
        printf("chdir(nonexistent) should have failed ENOENT, errno=%d\n", errno);
        return 1;
    }

    // chdir onto a regular file must fail ENOTDIR, and must NOT change cwd.
    int cfd = open("/tmp/chdir_target_file", O_CREAT | O_WRONLY, 0644);
    if (cfd < 0) { printf("open(chdir_target_file) FAILED\n"); return 1; }
    close(cfd);
    if (chdir("/tmp/chdir_target_file") == 0 || errno != ENOTDIR) {
        printf("chdir(file) should have failed ENOTDIR, errno=%d\n", errno);
        return 1;
    }

    if (chdir("/tmp") != 0) {
        printf("chdir(/tmp) FAILED: errno=%d\n", errno);
        return 1;
    }
    if (getcwd(cwdbuf, sizeof(cwdbuf)) == NULL || strcmp(cwdbuf, "/tmp") != 0) {
        printf("getcwd(after chdir /tmp) FAILED: got '%s'\n", cwdbuf);
        return 1;
    }

    // Relative path resolution: create+read a file using no leading '/'.
    int relfd = open("chdir_rel.txt", O_CREAT | O_TRUNC | O_WRONLY, 0644);
    if (relfd < 0) { printf("open(relative) FAILED: errno=%d\n", errno); return 1; }
    write(relfd, "rel", 3);
    close(relfd);
    struct stat relst;
    if (stat("/tmp/chdir_rel.txt", &relst) != 0) {
        printf("relative create didn't land in /tmp\n");
        return 1;
    }

    // ".." must actually walk up, not just get stripped.
    if (chdir("..") != 0) {
        printf("chdir(..) FAILED: errno=%d\n", errno);
        return 1;
    }
    if (getcwd(cwdbuf, sizeof(cwdbuf)) == NULL || strcmp(cwdbuf, "/") != 0) {
        printf("getcwd(after chdir ..) FAILED: got '%s'\n", cwdbuf);
        return 1;
    }

    // chdir() survives fork(): child sees the parent's cwd at fork time.
    if (chdir("/tmp") != 0) { printf("chdir(/tmp, 2nd) FAILED\n"); return 1; }
    pid_t cpid = fork();
    if (cpid < 0) { printf("fork(chdir) FAILED\n"); return 1; }
    if (cpid == 0) {
        char childbuf[64];
        if (getcwd(childbuf, sizeof(childbuf)) == NULL || strcmp(childbuf, "/tmp") != 0) {
            _exit(1);
        }
        _exit(0);
    }
    int cstatus = -1;
    waitpid(cpid, &cstatus, 0);
    if (!WIFEXITED(cstatus) || WEXITSTATUS(cstatus) != 0) {
        printf("child did not inherit cwd via fork\n");
        return 1;
    }

    if (unlink("/tmp/chdir_target_file") != 0 || unlink("/tmp/chdir_rel.txt") != 0) {
        printf("chdir_test cleanup FAILED\n");
        return 1;
    }
    printf("chdir_test: OK\n");

    return 0;
}
