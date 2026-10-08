// cap-exec: start a program in capability mode with only the descriptors it is given (docs/ux/handoff-capabilities-to-files.md
// stage 5). The one way services, preview providers and generated apps are started (P5): don't add another.
//
//   cap-exec [--dir PATH[:RIGHTS]]... [--fd N[:RIGHTS]]... [--stdio-all] -- PROG [ARG]...
//
//   --dir PATH[:RIGHTS]  open directory PATH and hand it over (fds 3, 4, ... in order, after any --fd numbers), limited to
//                        RIGHTS (default ro)
//   --fd N[:RIGHTS]      keep inherited descriptor N, at N, limited to RIGHTS (default ro)
//   --stdio-all          leave fds 0-2 their rights (by default stdin can only be read, stdout/stderr only written)
//   RIGHTS               ro | rx | rw, or rights joined by '+' (read+lookup+fstat, the names of CAP_* without the prefix)
//
// Every other descriptor is closed. The program learns what it got from CAPEXEC_FDS ("3=/tmp/x 4=/mnt/data 7=fd", by number). PROG is
// a path or a name looked up in PATH; it is opened before capability mode and run from that descriptor (execveat with
// AT_EMPTY_PATH), since exec by path is refused in the mode. Each failure says what failed, with the path.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include "constanos_capsicum.h"

extern char **environ;

static long sc5(long nr, long a, long b, long c, long d, long e) {
    long ret;
    register long r10 __asm__("r10") = d;
    register long r8 __asm__("r8") = e;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8) : "rcx", "r11", "memory");
    return ret;
}

#define RO (CAP_LOOKUP | CAP_READ | CAP_SEEK | CAP_FSTAT | CAP_EVENT | CAP_FCNTL | CAP_MMAP | CAP_FCHDIR)
#define RX (RO | CAP_FEXECVE)
#define RW (RO | CAP_WRITE | CAP_CREATE | CAP_MKDIRAT | CAP_UNLINKAT | CAP_FTRUNCATE | CAP_SYMLINKAT | CAP_FCHMOD | CAP_FUTIMES | \
            CAP_RENAMEAT_SOURCE | CAP_RENAMEAT_TARGET | CAP_LINKAT_SOURCE | CAP_LINKAT_TARGET)
// stdio keeps CAP_SEEK: libc's exit() seeks a stdio stream back over input it buffered but did not consume.
#define STDIN_RIGHTS  (CAP_READ | CAP_SEEK | CAP_EVENT | CAP_FSTAT | CAP_IOCTL | CAP_FCNTL)
#define STDOUT_RIGHTS (CAP_WRITE | CAP_SEEK | CAP_EVENT | CAP_FSTAT | CAP_IOCTL | CAP_FCNTL)

static const struct { const char *name; cap_rights_t bit; } NAMES[] = {
    {"read", CAP_READ}, {"write", CAP_WRITE}, {"seek", CAP_SEEK}, {"mmap", CAP_MMAP}, {"fstat", CAP_FSTAT},
    {"ftruncate", CAP_FTRUNCATE}, {"fchmod", CAP_FCHMOD}, {"futimes", CAP_FUTIMES}, {"ioctl", CAP_IOCTL},
    {"fcntl", CAP_FCNTL}, {"event", CAP_EVENT}, {"lookup", CAP_LOOKUP}, {"fchdir", CAP_FCHDIR}, {"create", CAP_CREATE},
    {"mkdirat", CAP_MKDIRAT}, {"symlinkat", CAP_SYMLINKAT}, {"unlinkat", CAP_UNLINKAT},
    {"renameat_source", CAP_RENAMEAT_SOURCE}, {"renameat_target", CAP_RENAMEAT_TARGET},
    {"linkat_source", CAP_LINKAT_SOURCE}, {"linkat_target", CAP_LINKAT_TARGET}, {"accept", CAP_ACCEPT},
    {"connect", CAP_CONNECT}, {"bind", CAP_BIND}, {"listen", CAP_LISTEN}, {"shutdown", CAP_SHUTDOWN},
    {"getpeername", CAP_GETPEERNAME}, {"getsockname", CAP_GETSOCKNAME}, {"getsockopt", CAP_GETSOCKOPT},
    {"setsockopt", CAP_SETSOCKOPT}, {"pdkill", CAP_PDKILL}, {"fexecve", CAP_FEXECVE},
};

static void die(const char *what, const char *arg, int err) {
    if (err) fprintf(stderr, "cap-exec: %s %s: %s\n", what, arg, strerror(err));
    else fprintf(stderr, "cap-exec: %s %s\n", what, arg);
    exit(127);
}

static void usage(void) {
    fprintf(stderr, "usage: cap-exec [--dir PATH[:RIGHTS]]... [--fd N[:RIGHTS]]... [--stdio-all] -- PROG [ARG]...\n"
                    "  RIGHTS: ro | rx | rw | name+name+... (read, write, lookup, create, ...)\n");
    exit(2);
}

static cap_rights_t parse_rights(const char *spec) {
    if (!strcmp(spec, "ro")) return RO;
    if (!strcmp(spec, "rx")) return RX;
    if (!strcmp(spec, "rw")) return RW;
    cap_rights_t r = 0;
    char buf[256];
    snprintf(buf, sizeof buf, "%s", spec);
    for (char *tok = strtok(buf, "+"); tok; tok = strtok(NULL, "+")) {
        size_t i;
        for (i = 0; i < sizeof NAMES / sizeof NAMES[0]; i++)
            if (!strcmp(tok, NAMES[i].name)) { r |= NAMES[i].bit; break; }
        if (i == sizeof NAMES / sizeof NAMES[0]) die("unknown right", tok, 0);
    }
    return r;
}

// "PATH[:RIGHTS]" -> PATH (in place) and the rights.
static cap_rights_t split_spec(char *s, cap_rights_t dflt) {
    char *colon = strrchr(s, ':');
    if (!colon) return dflt;
    *colon = 0;
    return parse_rights(colon + 1);
}

#define MAX_GRANTS 32
struct grant { int src; int dst; cap_rights_t rights; const char *label; };

static int open_program(const char *prog) {
    if (strchr(prog, '/')) {
        int fd = open(prog, O_RDONLY | O_CLOEXEC);
        if (fd < 0) die("cannot open program", prog, errno);
        return fd;
    }
    const char *path = getenv("PATH");
    char dirs[1024];
    snprintf(dirs, sizeof dirs, "%s", path ? path : "/bin:/mnt/bin");
    for (char *dir = strtok(dirs, ":"); dir; dir = strtok(NULL, ":")) {
        char full[512];
        snprintf(full, sizeof full, "%s/%s", dir, prog);
        int fd = open(full, O_RDONLY | O_CLOEXEC);
        if (fd >= 0) return fd;
    }
    die("program not found in PATH:", prog, 0);
    return -1;
}

int main(int argc, char **argv) {
    struct grant g[MAX_GRANTS];
    int n = 0, stdio_all = 0, i;
    for (i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--")) { i++; break; }
        if (n >= MAX_GRANTS) die("too many grants", "", 0);
        if (!strcmp(argv[i], "--dir") && i + 1 < argc) {
            char *spec = argv[++i];
            cap_rights_t r = split_spec(spec, RO);
            int fd = open(spec, O_RDONLY | O_DIRECTORY);
            if (fd < 0) die("cannot open directory", spec, errno);
            g[n++] = (struct grant){ fd, -1, r, spec };
        } else if (!strcmp(argv[i], "--fd") && i + 1 < argc) {
            char *spec = argv[++i];
            cap_rights_t r = split_spec(spec, RO);
            int fd = atoi(spec);
            if (fd < 3 || fcntl(fd, F_GETFD) < 0) die("not an open descriptor (3 or more):", spec, 0);
            g[n++] = (struct grant){ fd, fd, r, "fd" };
        } else if (!strcmp(argv[i], "--stdio-all")) {
            stdio_all = 1;
        } else {
            usage();
        }
    }
    if (i >= argc) usage();
    char **prog_argv = &argv[i];
    int progfd = open_program(prog_argv[0]);

    // Final numbers: --fd keeps its own; each --dir takes the lowest free one from 3. Then everything moves above the
    // table's top first, so a dup2 onto a final number never closes a source still needed.
    int taken[256] = {0};
    for (int k = 0; k < n; k++) if (g[k].dst >= 0) taken[g[k].dst] = 1;
    int next = 3;
    for (int k = 0; k < n; k++) {
        if (g[k].dst >= 0) continue;
        while (next < 256 && taken[next]) next++;
        if (next >= 200) die("too many directories", "", 0);
        g[k].dst = next;
        taken[next] = 1;
    }
    for (int k = 0; k < n; k++) {
        int hi = fcntl(g[k].src, F_DUPFD, 200);
        if (hi < 0) die("cannot move descriptor", g[k].label, errno);
        g[k].src = hi;
    }
    int prog_hi = fcntl(progfd, F_DUPFD_CLOEXEC, 200 + MAX_GRANTS);
    if (prog_hi < 0) die("cannot move the program's descriptor", prog_argv[0], errno);
    // Close everything from 3 up that is not one of the moved sources or the program.
    for (int fd = 3; fd < 256; fd++) {
        int keep = fd == prog_hi;
        for (int k = 0; k < n; k++) keep |= fd == g[k].src;
        if (!keep) close(fd);
    }
    for (int k = 0; k < n; k++) {
        if (dup2(g[k].src, g[k].dst) < 0) die("cannot place descriptor", g[k].label, errno);
        close(g[k].src);
        if (cap_rights_limit(g[k].dst, g[k].rights) != 0) die("cannot limit the rights of", g[k].label, errno);
    }
    // CAPEXEC_FDS lists them by descriptor number.
    char fds_env[1024] = "";
    for (int fd = 3; fd < 256; fd++) {
        for (int k = 0; k < n; k++) {
            if (g[k].dst != fd) continue;
            size_t len = strlen(fds_env);
            snprintf(fds_env + len, sizeof fds_env - len, "%s%d=%s", len ? " " : "", fd, g[k].label);
        }
    }
    if (!stdio_all) {
        cap_rights_limit(0, STDIN_RIGHTS);
        cap_rights_limit(1, STDOUT_RIGHTS);
        cap_rights_limit(2, STDOUT_RIGHTS);
    }
    setenv("CAPEXEC_FDS", fds_env, 1);

    long r = sc5(407, 0, 0, 0, 0, 0); // cap_enter
    if (r != 0) die("cap_enter failed:", "", (int)-r);
    r = sc5(322, prog_hi, (long)"", (long)prog_argv, (long)environ, 0x1000); // execveat(fd, "", argv, envp, AT_EMPTY_PATH)
    die("cannot run", prog_argv[0], (int)-r);
    return 127;
}
