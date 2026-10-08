// vfs/src/rights.rs
//
// Capability rights on file descriptors (Capsicum's model, `docs/ai/capabilities-plan.md`):
// every fd-table entry carries a mask of what may be done through it. `cap_rights_limit`
// only narrows it; `dup`, `fork`, `exec` and `SCM_RIGHTS` keep it; a file opened through a
// dirfd (`openat` and friends with a relative path) gets the dirfd's mask. A syscall that
// needs a right the fd lacks fails with `ENOTCAPABLE`.
//
// The names are Capsicum's (FreeBSD `sys/capsicum.h`); the bit layout is this kernel's own
// (one `u64`, not FreeBSD's versioned `cap_rights_t`). Pure: the kernel's fd table stores
// the masks and the syscall layer asks the functions below what a call needs.

use crate::types::Errno;

pub type Rights = u64;

pub const CAP_READ: Rights = 1 << 0;
pub const CAP_WRITE: Rights = 1 << 1;
pub const CAP_SEEK: Rights = 1 << 2;
/// `mmap` of the fd (plus `CAP_READ` for `PROT_READ`, `CAP_WRITE` for a shared writable map).
pub const CAP_MMAP: Rights = 1 << 3;
/// `fstat`, and `fstatat`/`statx`/`faccessat` through a dirfd (with `CAP_LOOKUP`).
pub const CAP_FSTAT: Rights = 1 << 4;
pub const CAP_FTRUNCATE: Rights = 1 << 5;
pub const CAP_FCHMOD: Rights = 1 << 6;
pub const CAP_FUTIMES: Rights = 1 << 7;
pub const CAP_IOCTL: Rights = 1 << 8;
/// `fcntl` `F_GETFL`/`F_SETFL` (the descriptor's own flags, `F_GETFD`/`F_SETFD`/`F_DUPFD*`, need nothing).
pub const CAP_FCNTL: Rights = 1 << 9;
/// `poll`, `epoll_ctl` and `epoll_wait`.
pub const CAP_EVENT: Rights = 1 << 10;
/// Use as the dirfd of an `*at` call with a relative path.
pub const CAP_LOOKUP: Rights = 1 << 11;
pub const CAP_FCHDIR: Rights = 1 << 12;
/// `openat(O_CREAT)` through a dirfd.
pub const CAP_CREATE: Rights = 1 << 13;
pub const CAP_MKDIRAT: Rights = 1 << 14;
pub const CAP_SYMLINKAT: Rights = 1 << 15;
/// `unlinkat` (files and, with `AT_REMOVEDIR`, directories).
pub const CAP_UNLINKAT: Rights = 1 << 16;
pub const CAP_RENAMEAT_SOURCE: Rights = 1 << 17;
pub const CAP_RENAMEAT_TARGET: Rights = 1 << 18;
pub const CAP_LINKAT_SOURCE: Rights = 1 << 19;
pub const CAP_LINKAT_TARGET: Rights = 1 << 20;
pub const CAP_ACCEPT: Rights = 1 << 21;
pub const CAP_CONNECT: Rights = 1 << 22;
pub const CAP_BIND: Rights = 1 << 23;
pub const CAP_LISTEN: Rights = 1 << 24;
pub const CAP_SHUTDOWN: Rights = 1 << 25;
pub const CAP_GETPEERNAME: Rights = 1 << 26;
pub const CAP_GETSOCKNAME: Rights = 1 << 27;
pub const CAP_GETSOCKOPT: Rights = 1 << 28;
pub const CAP_SETSOCKOPT: Rights = 1 << 29;
/// `pidfd_send_signal`.
pub const CAP_PDKILL: Rights = 1 << 30;

/// Every right: what a freshly opened descriptor has.
pub const CAP_ALL: Rights = (1 << 31) - 1;

/// The names, for the denial message and `/proc` (P1.1: say which right was missing).
const NAMES: [&str; 31] = [
    "CAP_READ", "CAP_WRITE", "CAP_SEEK", "CAP_MMAP", "CAP_FSTAT", "CAP_FTRUNCATE", "CAP_FCHMOD",
    "CAP_FUTIMES", "CAP_IOCTL", "CAP_FCNTL", "CAP_EVENT", "CAP_LOOKUP", "CAP_FCHDIR", "CAP_CREATE",
    "CAP_MKDIRAT", "CAP_SYMLINKAT", "CAP_UNLINKAT", "CAP_RENAMEAT_SOURCE", "CAP_RENAMEAT_TARGET",
    "CAP_LINKAT_SOURCE", "CAP_LINKAT_TARGET", "CAP_ACCEPT", "CAP_CONNECT", "CAP_BIND", "CAP_LISTEN",
    "CAP_SHUTDOWN", "CAP_GETPEERNAME", "CAP_GETSOCKNAME", "CAP_GETSOCKOPT", "CAP_SETSOCKOPT",
    "CAP_PDKILL",
];

/// Linux has no such error; this kernel's number, outside Linux's range (which ends at 133).
pub const ENOTCAPABLE: Errno = Errno(134);
/// Reserved for capability mode (stage 4 of `docs/ux/handoff-capabilities-to-files.md`).
pub const ECAPMODE: Errno = Errno(135);

/// `Ok` if `have` holds every right in `need`, else `ENOTCAPABLE`.
pub fn check(have: Rights, need: Rights) -> Result<(), Errno> {
    if need & !have == 0 { Ok(()) } else { Err(ENOTCAPABLE) }
}

/// The name of the lowest right in `need` that `have` lacks (`None` if none is missing).
pub fn first_missing(have: Rights, need: Rights) -> Option<&'static str> {
    let missing = need & !have & CAP_ALL;
    (missing != 0).then(|| NAMES[missing.trailing_zeros() as usize])
}

/// `cap_rights_limit`: the new mask, which may only drop rights. Asking for one the fd does not
/// have is `ENOTCAPABLE` (FreeBSD's answer); unknown bits are `EINVAL`.
pub fn limit(have: Rights, want: Rights) -> Result<Rights, Errno> {
    if want & !CAP_ALL != 0 {
        return Err(Errno::EINVAL);
    }
    check(have, want)?;
    Ok(want)
}

const O_ACCMODE: i32 = 3;
const O_WRONLY: i32 = 1;
const O_RDWR: i32 = 2;
const O_CREAT: i32 = 0o100;
const O_TRUNC: i32 = 0o1000;

/// The rights a dirfd needs to `openat` a relative path through it with `flags`: `CAP_LOOKUP`,
/// `CAP_READ` and/or `CAP_WRITE` for the access mode, `CAP_CREATE` for `O_CREAT`,
/// `CAP_FTRUNCATE` for `O_TRUNC` (FreeBSD's `openat(2)` rules).
pub fn openat_needs(flags: i32) -> Rights {
    let mut need = CAP_LOOKUP;
    match flags & O_ACCMODE {
        O_WRONLY => need |= CAP_WRITE,
        O_RDWR => need |= CAP_READ | CAP_WRITE,
        _ => need |= CAP_READ,
    }
    if flags & O_CREAT != 0 {
        need |= CAP_CREATE;
    }
    if flags & O_TRUNC != 0 {
        need |= CAP_FTRUNCATE;
    }
    need
}

const PROT_READ: u32 = 1;
const PROT_WRITE: u32 = 2;
const MAP_SHARED: u32 = 1;

/// The rights `mmap` of an fd needs: `CAP_MMAP`, plus `CAP_READ` for `PROT_READ` and `CAP_WRITE`
/// for `PROT_WRITE` on a shared mapping (a private one never writes back).
pub fn mmap_needs(prot: u32, flags: u32) -> Rights {
    let mut need = CAP_MMAP;
    if prot & PROT_READ != 0 {
        need |= CAP_READ;
    }
    if prot & PROT_WRITE != 0 && flags & MAP_SHARED != 0 {
        need |= CAP_WRITE;
    }
    need
}

/// The rights `fcntl(cmd)` needs: `CAP_FCNTL` for the file status flags, nothing for the
/// descriptor's own state.
pub fn fcntl_needs(cmd: i32) -> Rights {
    const F_GETFL: i32 = 3;
    const F_SETFL: i32 = 4;
    match cmd {
        F_GETFL | F_SETFL => CAP_FCNTL,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_needs_every_right() {
        assert_eq!(check(CAP_READ | CAP_WRITE, CAP_READ), Ok(()));
        assert_eq!(check(CAP_READ, CAP_READ | CAP_WRITE), Err(ENOTCAPABLE));
        assert_eq!(check(0, 0), Ok(()));
        assert_eq!(check(CAP_ALL, CAP_ALL), Ok(()));
    }

    #[test]
    fn limit_only_narrows() {
        assert_eq!(limit(CAP_ALL, CAP_READ), Ok(CAP_READ));
        assert_eq!(limit(CAP_READ | CAP_SEEK, CAP_READ), Ok(CAP_READ));
        assert_eq!(limit(CAP_READ, CAP_READ | CAP_WRITE), Err(ENOTCAPABLE));
        assert_eq!(limit(CAP_READ, 0), Ok(0));
        assert_eq!(limit(CAP_ALL, 1 << 40), Err(Errno::EINVAL));
    }

    #[test]
    fn missing_right_is_named() {
        assert_eq!(first_missing(CAP_READ, CAP_READ | CAP_WRITE), Some("CAP_WRITE"));
        assert_eq!(first_missing(CAP_ALL, CAP_PDKILL), None);
        assert_eq!(first_missing(0, CAP_PDKILL | CAP_ACCEPT), Some("CAP_ACCEPT"));
        assert_eq!(first_missing(0, CAP_PDKILL), Some("CAP_PDKILL"));
        // Every bit in CAP_ALL has a name.
        for bit in 0..31 {
            assert!(first_missing(0, 1 << bit).is_some(), "bit {bit}");
        }
    }

    #[test]
    fn openat_rights_follow_the_flags() {
        assert_eq!(openat_needs(0), CAP_LOOKUP | CAP_READ);
        assert_eq!(openat_needs(O_WRONLY), CAP_LOOKUP | CAP_WRITE);
        assert_eq!(openat_needs(O_RDWR), CAP_LOOKUP | CAP_READ | CAP_WRITE);
        assert_eq!(openat_needs(O_WRONLY | O_CREAT | O_TRUNC), CAP_LOOKUP | CAP_WRITE | CAP_CREATE | CAP_FTRUNCATE);
    }

    #[test]
    fn mmap_rights_follow_prot_and_sharing() {
        assert_eq!(mmap_needs(PROT_READ, 0), CAP_MMAP | CAP_READ);
        assert_eq!(mmap_needs(PROT_READ | PROT_WRITE, 0), CAP_MMAP | CAP_READ);
        assert_eq!(mmap_needs(PROT_READ | PROT_WRITE, MAP_SHARED), CAP_MMAP | CAP_READ | CAP_WRITE);
        assert_eq!(mmap_needs(0, MAP_SHARED), CAP_MMAP);
    }

    #[test]
    fn fcntl_rights() {
        assert_eq!(fcntl_needs(3), CAP_FCNTL);
        assert_eq!(fcntl_needs(4), CAP_FCNTL);
        assert_eq!(fcntl_needs(1), 0); // F_GETFD
        assert_eq!(fcntl_needs(0), 0); // F_DUPFD
    }
}
