// execve of a `#!` script (Linux's binfmt_script; rules in vfs::exec): the interpreter runs with
// [interpreter, optional arg, script path, argv[1..]], a script may name another script, a text file without `#!` is ENOEXEC
// (a shell then runs it itself), a missing interpreter is ENOENT, and `#!/bin/sh` works. The interpreter here is
// /mnt/bin/argv_test, which prints its argv; each child's output comes back through a pipe.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s: ", #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

static void put(const char *path, const char *text, int mode) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    write(fd, text, strlen(text));
    close(fd);
    chmod(path, mode);
}

// runs path with argv, the raw execve errno in *err (0 if it ran), its stdout in out
static int run(const char *path, char *const argv[], char *out, size_t cap, int *err) {
    int p[2], e[2];
    if (pipe(p) != 0 || pipe2(e, O_CLOEXEC) != 0) {
        printf("  pipe failed: %s\n", strerror(errno));
        failures++;
        return -1;
    }
    pid_t pid = fork();
    if (pid == 0) {
        dup2(p[1], 1);
        long r;
        char *envp[] = { "SHEBANG=1", NULL };
        __asm__ volatile("syscall" : "=a"(r) : "a"(59L), "D"(path), "S"(argv), "d"(envp) : "rcx", "r11", "memory");
        int code = (int)-r;
        write(e[1], &code, sizeof code);
        _exit(127);
    }
    close(p[1]);
    close(e[1]);
    size_t n = 0;
    ssize_t r;
    while (n + 1 < cap && (r = read(p[0], out + n, cap - 1 - n)) > 0) n += r;
    out[n] = 0;
    *err = 0;
    if (read(e[0], err, sizeof *err) != sizeof *err) *err = 0;
    int st;
    waitpid(pid, &st, 0);
    close(p[0]);
    close(e[0]);
    return st;
}

int main(void) {
    char out[2048];
    int err;
    mkdir("/tmp/sb", 0755);
    put("/tmp/sb/plain", "#!/mnt/bin/argv_test\n", 0755);
    put("/tmp/sb/witharg", "#!  /mnt/bin/argv_test   -x  y \nignored\n", 0755);
    put("/tmp/sb/nested", "#!/tmp/sb/plain\n", 0755);
    put("/tmp/sb/notext", "echo no shebang\n", 0755);
    put("/tmp/sb/missing", "#!/nope/interp\n", 0755);
    put("/tmp/sb/sh", "#!/bin/sh\necho from-sh \"$0\" \"$1\"\nexit 7\n", 0755);

    printf("a plain script: [interpreter, script, args...]\n");
    char *a1[] = { "plain", "one", "two", NULL };
    int st = run("/tmp/sb/plain", a1, out, sizeof out, &err);
    CHECK(err == 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0, "errno %d status 0x%x", err, st);
    CHECK(strstr(out, "argc=4\nargv[0]=/mnt/bin/argv_test\nargv[1]=/tmp/sb/plain\nargv[2]=one\nargv[3]=two\n") != NULL, "%s", out);
    CHECK(strstr(out, "envp[0]=SHEBANG=1") != NULL, "the environment is passed: %s", out);

    printf("the rest of the line is one argument, without the blanks around it\n");
    char *a2[] = { "witharg", NULL };
    run("/tmp/sb/witharg", a2, out, sizeof out, &err);
    CHECK(strstr(out, "argc=3\nargv[0]=/mnt/bin/argv_test\nargv[1]=-x  y\nargv[2]=/tmp/sb/witharg\n") != NULL, "%s", out);

    printf("a script for a script\n");
    char *a3[] = { "nested", "z", NULL };
    run("/tmp/sb/nested", a3, out, sizeof out, &err);
    CHECK(strstr(out, "argc=4\nargv[0]=/mnt/bin/argv_test\nargv[1]=/tmp/sb/plain\nargv[2]=/tmp/sb/nested\nargv[3]=z\n") != NULL, "%s", out);

    printf("no #!: ENOEXEC; a missing interpreter: ENOENT\n");
    char *a4[] = { "x", NULL };
    run("/tmp/sb/notext", a4, out, sizeof out, &err);
    CHECK(err == ENOEXEC, "errno %d", err);
    run("/tmp/sb/missing", a4, out, sizeof out, &err);
    CHECK(err == ENOENT, "errno %d", err);

    printf("#!/bin/sh runs BusyBox's ash\n");
    char *a5[] = { "sh-script", "arg1", NULL };
    st = run("/tmp/sb/sh", a5, out, sizeof out, &err);
    CHECK(err == 0 && WIFEXITED(st) && WEXITSTATUS(st) == 7, "errno %d status 0x%x", err, st);
    CHECK(strcmp(out, "from-sh /tmp/sb/sh arg1\n") == 0, "'%s'", out);

    printf(failures ? "shebang_test: FAIL\n" : "shebang_test: PASS\n");
    return failures != 0;
}
