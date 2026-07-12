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
}

fn run_program(name: &str) {
    let pid = syscall::fork();
    if pid == 0 {
        // Child: try to exec the requested program.
        syscall::with_cstr(name, |p| syscall::exec(p));
        // Only reached if exec failed.
        eprintln!("shell: unknown command: {}", name);
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
    } else {
        run_program(cmd);
    }
}

#[no_mangle]
extern "C" fn _start() -> ! {
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
