#![no_std]
#![no_main]

use userspace::{eprintln, print, println, syscall};

const BACKSPACE: u8 = 0x08;
const DEL: u8 = 0x7f;

/// Reads one line from stdin into `buf`, echoing each byte back to stdout
/// and handling backspace. Returns the number of bytes in the line
/// (not including the terminating newline).
fn read_line(buf: &mut [u8; 128]) -> usize {
    let mut len = 0usize;
    loop {
        let mut byte = [0u8; 1];
        let n = syscall::read(0, &mut byte);
        if n <= 0 {
            // Nothing to read right now; yield and try again.
            syscall::yield_now();
            continue;
        }
        let c = byte[0];
        match c {
            b'\n' | b'\r' => {
                print!("\n");
                break;
            }
            BACKSPACE | DEL => {
                if len > 0 {
                    len -= 1;
                    print!("\x08 \x08");
                }
            }
            _ => {
                if len < buf.len() {
                    buf[len] = c;
                    len += 1;
                    syscall::write(1, &byte);
                }
            }
        }
    }
    len
}

fn trim(s: &str) -> &str {
    s.trim()
}

fn print_help() {
    println!("Available commands:");
    println!("  help        - show this help message");
    println!("  exit        - exit the shell");
    println!("  uname       - print system name");
    println!("  ls          - list directory contents");
    println!("  uptime      - show system uptime");
    println!("  sleep       - sleep demo");
    println!("  tsc         - timestamp counter demo");
    println!("  snake       - play snake");
    println!("  ipc_ping    - IPC round-trip demo");
    println!("  mmap_test   - mmap/munmap demo");
    println!("  poll_test   - poll demo");
    println!("  hello       - hello world demo");
    println!("  pthread_test - pthread_create/join smoke test");
    println!("  producer_consumer - pthread_cond_t producer/consumer test");
    println!("  pipe_test   - pipe(2) fork/read/write/EOF test");
    println!("  signal_test - kill/sigaction/sigreturn/SIGCHLD test");
    println!("  mlibc_signal_test - same, via real mlibc pipe/kill/sigaction");
    println!("  write <path> - capture lines from stdin into a /tmp file (end with '.')");
    println!("  sh <path>   - run each line of a file as a shell command (batch mode)");
    println!("  meminfo     - show free physical memory (KiB)");
    println!("  cat <path>  - print a file's contents");
    println!("  ls <path>   - list any directory (plain 'ls' still lists /)");
    println!("  demo        - guided tour: VFS/ext2, threads, IPC, pipes, signals");
}

/// A pending fd redirection, parsed out of the command line before exec.
/// Either "dup `dup_src` onto `target_fd`" (`2>&1`) or "open the file at
/// `scratch[path_off..path_off+path_len]` (NUL included) with `flags` and
/// dup2 the result onto `target_fd`" (`>`, `>>`, `<`, `2>`, `2>>`).
#[derive(Clone, Copy)]
struct Redirect {
    target_fd: i32,
    dup_src:   i32, // >= 0 for the "dup an fd" form, -1 for "open a file"
    path_off:  usize,
    path_len:  usize,
    flags:     i32,
}

/// Runs `cmd` (the whole typed line, e.g. `"ls /tmp > /tmp/out.txt"`).
///
/// The first non-redirect word is both the program name to exec and
/// argv[0], the rest becomes argv[1..]. Recognizes `>`, `>>`, `<`, `2>`,
/// `2>>`, `2>&1`, `1>&2` as whitespace-separated tokens (no `cmd>file`
/// without spaces — this parser is line-oriented, not a real tokenizer).
/// No `alloc` in this userspace crate, so both argv and the redirect
/// target paths are built out of one flat stack buffer instead of a Vec —
/// each word gets NUL-terminated in place and sliced back out of it.
///
/// Redirection only ever applies to the forked child, exactly like a real
/// shell: `dup2` happens after `fork()` and before `exec()`, so the
/// shell's own stdin/stdout/stderr are never touched and need no
/// restoring afterward.
fn run_program(cmd: &str) {
    const MAX_ARGS: usize = 16;
    const MAX_REDIRECTS: usize = 4;
    const SCRATCH_SIZE: usize = 512;

    let mut scratch = [0u8; SCRATCH_SIZE];
    let mut cursor = 0usize;

    let mut arg_offsets = [(0usize, 0usize); MAX_ARGS]; // (start, len incl. NUL)
    let mut argc = 0usize;
    let mut prog_name: &str = "";

    let mut redirects = [Redirect { target_fd: -1, dup_src: -1, path_off: 0, path_len: 0, flags: 0 }; MAX_REDIRECTS];
    let mut nredirs = 0usize;

    let mut words = cmd.split_whitespace();
    while let Some(word) = words.next() {
        // (target fd, append?, read-instead-of-write?, dup-not-file?)
        let op = match word {
            ">"    => Some((1, false, false, false)),
            ">>"   => Some((1, true,  false, false)),
            "<"    => Some((0, false, true,  false)),
            "2>"   => Some((2, false, false, false)),
            "2>>"  => Some((2, true,  false, false)),
            "2>&1" => Some((2, false, false, true)),
            "1>&2" => Some((1, false, false, true)),
            _ => None,
        };

        let Some((target_fd, append, is_input, is_dup)) = op else {
            // Plain argv word.
            if argc < MAX_ARGS {
                let bytes = word.as_bytes();
                let n = bytes.len().min(SCRATCH_SIZE.saturating_sub(cursor + 1));
                scratch[cursor..cursor + n].copy_from_slice(&bytes[..n]);
                scratch[cursor + n] = 0; // NUL terminator
                if argc == 0 { prog_name = word; }
                arg_offsets[argc] = (cursor, n + 1);
                cursor += n + 1;
                argc += 1;
            }
            continue;
        };

        if nredirs >= MAX_REDIRECTS { continue; }

        if is_dup {
            let dup_src = if target_fd == 2 { 1 } else { 2 };
            redirects[nredirs] = Redirect { target_fd, dup_src, path_off: 0, path_len: 0, flags: 0 };
            nredirs += 1;
            continue;
        }

        let path = match words.next() {
            Some(p) => p,
            None => { eprintln!("shell: {}: missing filename", word); return; }
        };
        let bytes = path.as_bytes();
        let n = bytes.len().min(SCRATCH_SIZE.saturating_sub(cursor + 1));
        let path_off = cursor;
        scratch[cursor..cursor + n].copy_from_slice(&bytes[..n]);
        scratch[cursor + n] = 0;
        cursor += n + 1;

        let flags = if is_input {
            syscall::O_RDONLY
        } else if append {
            syscall::O_CREAT | syscall::O_APPEND | syscall::O_WRONLY
        } else {
            syscall::O_CREAT | syscall::O_TRUNC | syscall::O_WRONLY
        };
        redirects[nredirs] = Redirect { target_fd, dup_src: -1, path_off, path_len: n + 1, flags };
        nredirs += 1;
    }

    if argc == 0 {
        return;
    }

    let mut argv: [&[u8]; MAX_ARGS] = [&[]; MAX_ARGS];
    for i in 0..argc {
        let (start, len) = arg_offsets[i];
        argv[i] = &scratch[start..start + len];
    }

    let pid = syscall::fork();
    if pid == 0 {
        for i in 0..nredirs {
            let r = redirects[i];
            let src_fd = if r.dup_src >= 0 {
                r.dup_src
            } else {
                let path_cstr = &scratch[r.path_off..r.path_off + r.path_len];
                let fd = syscall::open(path_cstr, r.flags) as i32;
                if fd < 0 {
                    let path = core::str::from_utf8(&path_cstr[..r.path_len - 1]).unwrap_or("?");
                    eprintln!("shell: cannot open {}", path);
                    syscall::exit(1);
                }
                fd
            };
            syscall::dup2(src_fd, r.target_fd);
            if r.dup_src < 0 && src_fd != r.target_fd {
                syscall::close(src_fd);
            }
        }

        // Child: try to exec the requested program.
        syscall::exec_argv(argv[0], &argv[..argc], &[]);
        // Only reached if exec failed.
        eprintln!("shell: unknown command: {}", prog_name);
        syscall::exit(1);
    } else if pid > 0 {
        // Parent: wait for the child to finish.
        syscall::waitpid(pid);
    } else {
        println!("shell: fork failed ({})", pid);
    }
}

/// `write <path>`: captures lines typed at the prompt into a file (created
/// fresh each time — O_CREAT|O_TRUNC) until a line containing just "." is
/// entered. Meant for /tmp (the only writable mount) — e.g. build up a
/// batch script once, then replay it instantly with `sh` instead of
/// re-typing/piping the same commands over serial every time.
fn cmd_write(path: &str) {
    if path.is_empty() {
        eprintln!("write: usage: write <path>");
        return;
    }
    let flags = syscall::O_CREAT | syscall::O_WRONLY | syscall::O_TRUNC;
    let fd = syscall::with_cstr(path, |p| syscall::open(p, flags));
    if fd < 0 {
        eprintln!("write: cannot create {} ({})", path, fd);
        return;
    }
    let fd = fd as i32;
    println!("Writing to {} — a line with just '.' ends it.", path);

    let mut line_buf = [0u8; 128];
    loop {
        print!("> ");
        let len = read_line(&mut line_buf);
        let raw = core::str::from_utf8(&line_buf[..len]).unwrap_or("");
        if trim(raw) == "." {
            break;
        }
        syscall::write(fd, raw.as_bytes());
        syscall::write(fd, b"\n");
    }
    syscall::close(fd);
    println!("OK");
}

/// `sh <path>`: batch mode — reads the whole file, then dispatches each
/// non-empty line the same way the interactive prompt would, echoing "$
/// <line>" first so the transcript reads the same either way.
fn cmd_sh(path: &str) {
    if path.is_empty() {
        eprintln!("sh: usage: sh <path>");
        return;
    }
    let fd = syscall::with_cstr(path, |p| syscall::open(p, syscall::O_RDONLY));
    if fd < 0 {
        eprintln!("sh: cannot open {} ({})", path, fd);
        return;
    }
    let fd = fd as i32;

    let mut content = [0u8; 4096];
    let mut total = 0usize;
    while total < content.len() {
        let n = syscall::read(fd, &mut content[total..]);
        if n <= 0 {
            break;
        }
        total += n as usize;
    }
    syscall::close(fd);

    let text = core::str::from_utf8(&content[..total]).unwrap_or("");
    for line in text.split('\n') {
        let line = trim(line);
        if line.is_empty() {
            continue;
        }
        println!("$ {}", line);
        dispatch(line);
    }
}

/// `cat <path>`: prints a file's contents to stdout. Mainly exists to
/// exercise/verify fs::ext2's read path from the shell (`ls <path>` shows
/// entries exist; this proves the actual bytes come back right too) —
/// `ls`/other embedded programs don't take argv (see README: "no argv/envp
/// support"), so this and `ls <path>` below are shell built-ins instead of
/// real programs.
fn cmd_cat(path: &str) {
    if path.is_empty() {
        eprintln!("cat: usage: cat <path>");
        return;
    }
    let fd = syscall::with_cstr(path, |p| syscall::open(p, syscall::O_RDONLY));
    if fd < 0 {
        eprintln!("cat: cannot open {} ({})", path, fd);
        return;
    }
    let fd = fd as i32;
    let mut buf = [0u8; 256];
    loop {
        let n = syscall::read(fd, &mut buf);
        if n <= 0 {
            break;
        }
        syscall::write(1, &buf[..n as usize]);
    }
    syscall::close(fd);
}

fn dirent_type_marker(d_type: u8) -> &'static str {
    match d_type {
        4 => "/", // DT_DIR
        2 => "@", // DT_CHR
        8 => "",  // DT_REG
        _ => "?",
    }
}

/// `ls <path>`: same getdents64 loop as the standalone `ls` program, just
/// able to target an arbitrary path (e.g. `ls /mnt`) since built-ins get
/// the raw command tail as an argument and real programs don't.
fn cmd_ls(path: &str) {
    let fd = syscall::with_cstr(path, |p| syscall::open(p, 0));
    if fd < 0 {
        eprintln!("ls: cannot open {}: {}", path, fd);
        return;
    }
    let fd = fd as i32;
    let mut buf = [0u8; 512];
    loop {
        let n = syscall::getdents64(fd, &mut buf);
        if n <= 0 {
            break;
        }
        let n = n as usize;
        let mut off = 0usize;
        while off < n {
            match syscall::parse_dirent(&buf[off..n]) {
                Some(entry) => {
                    let name = core::str::from_utf8(entry.name).unwrap_or("?");
                    println!("{}{}", name, dirent_type_marker(entry.d_type));
                    if entry.record_len == 0 {
                        break;
                    }
                    off += entry.record_len;
                }
                None => break,
            }
        }
    }
    syscall::close(fd);
}

fn dispatch(cmd: &str) {
    if cmd.is_empty() {
        return;
    } else if cmd == "help" {
        print_help();
    } else if cmd == "exit" {
        syscall::exit(0);
    } else if let Some(path) = cmd.strip_prefix("write ") {
        cmd_write(trim(path));
    } else if let Some(path) = cmd.strip_prefix("sh ") {
        cmd_sh(trim(path));
    } else if cmd == "meminfo" {
        println!("free: {} KiB", syscall::meminfo_kb());
    } else if let Some(path) = cmd.strip_prefix("cat ") {
        cmd_cat(trim(path));
    } else if let Some(path) = cmd.strip_prefix("ls ") {
        cmd_ls(trim(path));
    } else {
        run_program(cmd);
    }
}

#[no_mangle]
extern "C" fn _start() -> ! {
    // BusyBox ash (real job control, line editing, standalone/nofork applet
    // dispatch — see the busybox-readiness session notes for how it got
    // there) is the default interactive shell now. This process (PID 1)
    // stays alive as ash's parent/waitpid()er and, if ash ever exits (its
    // own `exit`, Ctrl-D, or a crash), falls back into this hand-rolled
    // REPL below instead of leaving the system with no way to type
    // anything — same fork/exec/waitpid path as any other `run_program`
    // call, just invoked once up front instead of from typed input.
    run_program("busybox ash");

    println!("ConstanOS shell");
    println!("Type 'help' for a list of commands.");

    let mut line_buf = [0u8; 128];
    loop {
        print!("$ ");
        let len = read_line(&mut line_buf);
        let raw = core::str::from_utf8(&line_buf[..len]).unwrap_or("");
        dispatch(trim(raw));
    }
}
