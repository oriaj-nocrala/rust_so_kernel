#![no_std]
#![no_main]

use userspace::{eprintln, syscall};

const MESSAGE: &str = "the quick brown fox jumps over the lazy dog";

#[no_mangle]
extern "C" fn _start() -> ! {
    let (rfd, wfd) = match syscall::pipe() {
        Ok(fds) => fds,
        Err(e) => {
            eprintln!("pipe_test: pipe() failed ({})", e);
            syscall::exit(1);
        }
    };

    let pid = syscall::fork();
    if pid == 0 {
        // Child: write end only.
        syscall::close(rfd);
        let n = syscall::write(wfd, MESSAGE.as_bytes());
        if n != MESSAGE.len() as i64 {
            eprintln!("pipe_test: child write returned {}, expected {}", n, MESSAGE.len());
            syscall::exit(1);
        }
        syscall::close(wfd);
        syscall::exit(0);
    } else if pid < 0 {
        eprintln!("pipe_test: fork() failed ({})", pid);
        syscall::exit(1);
    }

    // Parent: read end only.
    syscall::close(wfd);

    let mut received = [0u8; 128];
    let mut total = 0usize;
    loop {
        let n = syscall::read(rfd, &mut received[total..]);
        if n < 0 {
            eprintln!("pipe_test: read failed ({})", n);
            syscall::exit(1);
        }
        if n == 0 {
            break; // EOF: writer closed
        }
        total += n as usize;
    }
    syscall::close(rfd);
    syscall::waitpid(pid);

    let got = core::str::from_utf8(&received[..total]).unwrap_or("<invalid utf8>");
    if got == MESSAGE {
        eprintln!("pipe_test: PASS ({} bytes, matched)", total);
    } else {
        eprintln!("pipe_test: FAIL - got {:?} ({} bytes), want {:?}", got, total, MESSAGE);
    }

    syscall::exit(0);
}
