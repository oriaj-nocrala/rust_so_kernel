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
///
/// Only boot-critical/small programs live here — `shell` (PID 1), `busybox`
/// (ash + every applet, on the boot path via `busybox --install`), the
/// small Rust smoke tests (all well under 50 KiB), and `kdebug` (the live
/// tracing-control tool used in an ongoing debugging investigation
/// alongside busybox — see CLAUDE.md). Everything else runnable-but-not-
/// boot-critical (`doom`, `quake`, and most of the old C test programs —
/// `hello`, `pthread_test`, `producer_consumer`, `mlibc_signal_test`,
/// `stat_test`, `argv_test`, `jobctl_test`, `ext2_robust_test`, `fpu_test`)
/// was moved off the kernel binary entirely and now lives at `/mnt/bin/` on
/// the ext2 disk image instead (built straight there by `kernel/build.rs`'s
/// `DISK_C_PROGRAMS`/`DOOM_NAME`/`QUAKE_NAME`, synced onto `disk.img` by the
/// root `build.rs`'s `sync_disk_bin_dir`). `sys_exec` already resolves
/// through the real VFS (see CLAUDE.md's Userspace Programs section), so
/// they run identically once `$PATH` (set by `userspace/src/bin/shell.rs`)
/// includes `/mnt/bin` — no special-casing needed here, they just aren't
/// registered in this table at all, and so don't show up in initramfs's
/// `/bin` (`ls /bin`) either, only in `/mnt/bin`.
static PROGRAMS: [(&str, ProgramSource); 13] = [
    ("uname",     ProgramSource::Elf(include_bytes!("../../embedded/uname.elf"))),
    ("shell",     ProgramSource::Elf(include_bytes!("../../embedded/shell.elf"))),
    ("snake",     ProgramSource::Elf(include_bytes!("../../embedded/snake.elf"))),
    ("uptime",    ProgramSource::Elf(include_bytes!("../../embedded/uptime.elf"))),
    ("tsc",       ProgramSource::Elf(include_bytes!("../../embedded/tsc.elf"))),
    ("ipc_ping",  ProgramSource::Elf(include_bytes!("../../embedded/ipc_ping.elf"))),
    ("mmap_test", ProgramSource::Elf(include_bytes!("../../embedded/mmap_test.elf"))),
    ("poll_test", ProgramSource::Elf(include_bytes!("../../embedded/poll_test.elf"))),
    ("pipe_test", ProgramSource::Elf(include_bytes!("../../embedded/pipe_test.elf"))),
    ("signal_test", ProgramSource::Elf(include_bytes!("../../embedded/signal_test.elf"))),
    ("demo",      ProgramSource::Elf(include_bytes!("../../embedded/demo.elf"))),
    ("kdebug",    ProgramSource::Elf(include_bytes!("../../embedded/kdebug.elf"))),
    // Manually vendored (not built by kernel/build.rs — no Makefile-based
    // C_PROGRAMS support yet): busybox-1.36.1 built out-of-tree against
    // sysroot/ with CONFIG_TRUE=y (only the `true` applet) as a first
    // smoke test. See the busybox-readiness memory / session notes for
    // the exact cross-compile recipe and every sysroot header gap it took
    // to get this far.
    ("busybox",   ProgramSource::Elf(include_bytes!("../../embedded/busybox.elf"))),
];

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