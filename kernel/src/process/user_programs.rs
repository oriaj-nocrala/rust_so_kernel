// kernel/src/process/user_programs.rs
//
// Embedded user-space programs.
//
// For now, programs are compiled separately and embedded into the
// kernel binary via `include_bytes!`.  In the future, these would
// be loaded from a filesystem.
//
// BUILD WORKFLOW:
//   1. Write your program in userspace/src/bin/<name>.rs
//   2. Build:  cd userspace && cargo build --release
//   3. The ELF lands in userspace/target/x86_64-unknown-none/release/<name>
//   4. Copy it to kernel/embedded/<name>.elf
//   5. Rebuild kernel — include_bytes! picks it up
//
// FALLBACK:
//   If no ELF binary exists at the expected path, the build will fail.
//   To bootstrap without a userspace binary, use `get_fallback_test()`
//   which returns a pointer to the inline assembly tests from
//   user_test_fileio.rs (the old approach).

/// Embedded "hello" ELF binary.
///
/// This is the primary user program.  Build it with the userspace crate,
/// then place the ELF at kernel/embedded/hello.elf.
///
/// If the file doesn't exist, comment out this line and use the fallback.
// pub static HELLO_ELF: &[u8] = include_bytes!("../../embedded/hello.elf");

/// List of available embedded programs.
///
/// Each entry is (name, elf_bytes).  Used by init/processes.rs to
/// create user processes.
pub fn list_programs() -> &'static [(&'static str, ProgramSource)] {
    &PROGRAMS
}

/// How a user program is provided to the loader.
pub enum ProgramSource {
    /// Raw ELF bytes (from include_bytes! or a filesystem read).
    Elf(&'static [u8]),
    /// Legacy: raw code pointer + size for inline assembly tests.
    /// Used as a fallback until ELF userspace is ready.
    RawCode {
        code_ptr: fn() -> *const u8,
        code_size: usize,
    },
}

/// Registry of embedded programs.
///
/// BOOTSTRAPPING:
///   Start with RawCode entries pointing to user_test_fileio tests.
///   Once you have a userspace ELF, switch to Elf entries.
///
/// To add an ELF program:
///   1. Build it (see workflow above)
///   2. Add: ("name", ProgramSource::Elf(include_bytes!("../../embedded/name.elf")))
static PROGRAMS: [(&str, ProgramSource); 24] = [
    ("uname",     ProgramSource::Elf(include_bytes!("../../embedded/uname.elf"))),
    ("shell",     ProgramSource::Elf(include_bytes!("../../embedded/shell.elf"))),
    ("snake",     ProgramSource::Elf(include_bytes!("../../embedded/snake.elf"))),
    ("uptime",    ProgramSource::Elf(include_bytes!("../../embedded/uptime.elf"))),
    ("tsc",       ProgramSource::Elf(include_bytes!("../../embedded/tsc.elf"))),
    ("ipc_ping",  ProgramSource::Elf(include_bytes!("../../embedded/ipc_ping.elf"))),
    ("mmap_test", ProgramSource::Elf(include_bytes!("../../embedded/mmap_test.elf"))),
    ("poll_test", ProgramSource::Elf(include_bytes!("../../embedded/poll_test.elf"))),
    ("hello",     ProgramSource::Elf(include_bytes!("../../embedded/hello.elf"))),
    ("pthread_test", ProgramSource::Elf(include_bytes!("../../embedded/pthread_test.elf"))),
    ("producer_consumer", ProgramSource::Elf(include_bytes!("../../embedded/producer_consumer.elf"))),
    ("pipe_test", ProgramSource::Elf(include_bytes!("../../embedded/pipe_test.elf"))),
    ("signal_test", ProgramSource::Elf(include_bytes!("../../embedded/signal_test.elf"))),
    ("mlibc_signal_test", ProgramSource::Elf(include_bytes!("../../embedded/mlibc_signal_test.elf"))),
    ("demo",      ProgramSource::Elf(include_bytes!("../../embedded/demo.elf"))),
    ("stat_test", ProgramSource::Elf(include_bytes!("../../embedded/stat_test.elf"))),
    ("argv_test", ProgramSource::Elf(include_bytes!("../../embedded/argv_test.elf"))),
    ("jobctl_test", ProgramSource::Elf(include_bytes!("../../embedded/jobctl_test.elf"))),
    ("kdebug",    ProgramSource::Elf(include_bytes!("../../embedded/kdebug.elf"))),
    ("ext2_robust_test", ProgramSource::Elf(include_bytes!("../../embedded/ext2_robust_test.elf"))),
    ("fpu_test", ProgramSource::Elf(include_bytes!("../../embedded/fpu_test.elf"))),
    // Manually vendored (not built by kernel/build.rs — no Makefile-based
    // C_PROGRAMS support yet): busybox-1.36.1 built out-of-tree against
    // sysroot/ with CONFIG_TRUE=y (only the `true` applet) as a first
    // smoke test. See the busybox-readiness memory / session notes for
    // the exact cross-compile recipe and every sysroot header gap it took
    // to get this far.
    ("busybox",   ProgramSource::Elf(include_bytes!("../../embedded/busybox.elf"))),
    // doomgeneric (git submodule) + doom-port/doomgeneric_constanos.c (our
    // platform port), built by scripts/build-doom.sh — same "external
    // multi-file build, not the single-file C_PROGRAMS loop" shape as
    // BusyBox above.
    ("doom",      ProgramSource::Elf(include_bytes!("../../embedded/doom.elf"))),
    // quakegeneric (git submodule) + quake-port/quakegeneric_constanos.c
    // (our platform port), built by scripts/build-quake.sh — same shape
    // as doom.elf above.
    ("quake",     ProgramSource::Elf(include_bytes!("../../embedded/quake.elf"))),
];

/// Stack size (in 4 KiB pages) `sys_exec` should request for a program
/// resolved to `exe_name` (its canonical VFS path, e.g. `/bin/quake`) —
/// see `memory::elf_loader::load_elf_with_stack_pages`. Everything gets
/// the loader's own default (64 KiB) except entries listed here.
///
/// This exists as a narrow, per-program override rather than a raised
/// global default: `elf_loader::STACK_PAGES`'s doc comment has the full
/// story, but in short, bumping the default itself (every process, not
/// just the one that needs it) reproducibly hung boot partway through
/// `busybox --install`'s own `fork()` — a real, if not fully root-caused,
/// regression. Confining the bigger stack to just the program that
/// actually needs it (Quake's software renderer — `Host_Init`'s call
/// chain overflows the 64 KiB default) avoids destabilizing every other
/// already-stable process.
pub fn stack_pages_for(exe_name: &str) -> usize {
    const LARGE_STACK_PAGES: usize = 256; // 1 MiB
    if exe_name.rsplit('/').next() == Some("quake") {
        LARGE_STACK_PAGES
    } else {
        16 // matches elf_loader::STACK_PAGES's default
    }
}

/// Print available programs to serial.
pub fn print_available() {
    crate::serial_println!("📦 Embedded user programs:");
    for (name, source) in PROGRAMS.iter() {
        let kind = match source {
            ProgramSource::Elf(data) => alloc::format!("ELF ({} bytes)", data.len()),
            ProgramSource::RawCode { code_size, .. } => {
                alloc::format!("raw asm ({} bytes)", code_size)
            }
        };
        crate::serial_println!("  '{}' — {}", name, kind);
    }
}