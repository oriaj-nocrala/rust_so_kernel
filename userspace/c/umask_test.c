// umask(95): the mask starts at Linux's 022, umask() returns the previous one and keeps only 0777, a forked child and an exec'd
// program inherit it. Through libc (mlibc's sys_umask sysdep) and through the raw syscall, so both the port and the kernel are covered.
// The kernel does not apply the mask yet (open/mkdir keep no mode): this checks the bookkeeping only.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/stat.h>
#include <sys/wait.h>

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

static long raw_umask(long mask) {
    long r;
    __asm__ volatile("syscall" : "=a"(r) : "a"(95L), "D"(mask) : "rcx", "r11", "memory");
    return r;
}

int main(int argc, char **argv) {
    // exec'd by the test below: print the mask it was started with
    if (argc == 2 && strcmp(argv[1], "--print") == 0) {
        mode_t m = umask(0);
        printf("%03o\n", m);
        return 0;
    }
    printf("the boot mask is 022 (this test is started by ash from PID 1, neither changes it)\n");
    long first = raw_umask(022);
    CHECK(first == 022, "first umask returned %lo", first);

    printf("umask() returns the previous mask, through libc and raw\n");
    CHECK(umask(027) == 022, "libc umask did not return 022");
    CHECK(raw_umask(077) == 027, "raw umask did not return 027");
    CHECK(umask(027) == 077, "libc umask did not return 077");

    printf("only the permission bits are kept\n");
    raw_umask(07777);
    CHECK(raw_umask(027) == 0777, "07777 was not kept as 0777");

    printf("a forked child inherits the mask, and its change stays its own\n");
    int p[2];
    CHECK(pipe(p) == 0, "pipe failed");
    pid_t pid = fork();
    if (pid == 0) {
        char c = (char)umask(0);
        write(p[1], &c, 1);
        _exit(0);
    }
    char got = 0;
    read(p[0], &got, 1);
    waitpid(pid, NULL, 0);
    CHECK((unsigned char)got == 027, "the child saw %o", (unsigned char)got);
    CHECK(umask(027) == 027, "the child's umask(0) reached the parent");

    printf("an exec'd program starts with the mask\n");
    int q[2];
    pipe(q);
    pid = fork();
    if (pid == 0) {
        dup2(q[1], 1);
        execl("/proc/self/exe", "umask_test", "--print", (char *)NULL);
        _exit(127);
    }
    close(q[1]);
    char buf[16] = {0};
    read(q[0], buf, sizeof buf - 1);
    int st = 0;
    waitpid(pid, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "the exec'd test ended with 0x%x", st);
    CHECK(strncmp(buf, "027", 3) == 0, "the exec'd program printed '%s'", buf);

    umask(022);
    printf(failures ? "umask_test: FAIL\n" : "umask_test: PASS\n");
    return failures != 0;
}
