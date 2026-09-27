//! Whole-file helpers over the raw syscalls.

use alloc::vec::Vec;

use crate::syscall;

/// The whole of `path`, or the negative errno of the `open`.
pub fn read_file(path: &str) -> Result<Vec<u8>, i64> {
    let fd = syscall::with_cstr(path, |p| syscall::open(p, syscall::O_RDONLY));
    if fd < 0 {
        return Err(fd);
    }
    let fd = fd as i32;
    let mut buf = Vec::new();
    if let Ok(st) = syscall::fstat(fd) {
        buf.reserve_exact(st.st_size as usize);
    }
    let mut chunk = [0u8; 16384];
    loop {
        let n = syscall::read(fd, &mut chunk);
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
    }
    syscall::close(fd);
    Ok(buf)
}
