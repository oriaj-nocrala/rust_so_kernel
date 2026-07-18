// Smoke test for real argv/envp support in exec() (kernel/src/process/
// syscall.rs::sys_exec + kernel/src/memory/elf_loader.rs's dynamic
// argc/argv/envp/auxv stack frame). Uses the standard three-argument
// main() so this also proves mlibc's crt1.S / __dlapi_enter need zero
// changes to pick up real arguments once the kernel emits a real stack.
#include <stdio.h>

int main(int argc, char **argv, char **envp) {
    printf("argc=%d\n", argc);
    for (int i = 0; i < argc; i++) {
        printf("argv[%d]=%s\n", i, argv[i]);
    }

    int envc = 0;
    for (char **e = envp; *e != NULL; e++) {
        printf("envp[%d]=%s\n", envc, *e);
        envc++;
    }
    printf("envc=%d\n", envc);
    printf("argv_test: OK\n");
    return 0;
}
