
#include <stdint.h>
#include <stdlib.h>
#include <bits/ensure.h>
#include <mlibc/elf/startup.h>

extern "C" void __dlapi_enter(uintptr_t *);

extern char **environ;

// __dlapi_enter() runs the same static-TLS/auxv bootstrap the dynamic linker
// uses (interpreterMain(), compiled into libc.a under -DMLIBC_STATIC_BUILD
// whenever default_library=static — see mlibc/meson.build). It MUST run
// before main(): without it, FS.base is never set up, so any mlibc internal
// %fs-relative access (errno, stdio locks, ...) reads/writes near address 0
// and immediately segfaults. Several reference sysdeps ports (dripos, aero,
// lemon, keyronex) skip this call with a "TODO" comment and are therefore
// broken for static binaries that touch TLS — don't copy that gap again.
extern "C" void __mlibc_entry(int (*main_fn)(int argc, char *argv[], char *env[]), uintptr_t *entry_stack) {
	__dlapi_enter(entry_stack);
	auto result = main_fn(mlibc::entry_stack.argc, mlibc::entry_stack.argv, environ);
	exit(result);
}

