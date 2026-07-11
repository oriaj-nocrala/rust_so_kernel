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

#[no_mangle]
extern "C" fn _start() -> ! {
    println!("ConstanOS shell");
    println!("Type 'help' for a list of commands.");

    let mut line_buf = [0u8; 128];
    loop {
        print!("$ ");
        let len = read_line(&mut line_buf);
        let raw = core::str::from_utf8(&line_buf[..len]).unwrap_or("");
        let cmd = trim(raw);

        if cmd.is_empty() {
            continue;
        } else if cmd == "help" {
            print_help();
        } else if cmd == "exit" {
            syscall::exit(0);
        } else {
            run_program(cmd);
        }
    }
}
