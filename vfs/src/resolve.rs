// vfs/src/resolve.rs
//
// openat2(2)'s argument: `struct open_how` and its `RESOLVE_*` bits, with
// Linux's validation rules (fs/open.c `build_open_how`/`build_open_flags`,
// kernel/sys.c `copy_struct_from_user`). Pure: the syscall layer copies the
// user bytes and calls `OpenHow::parse`; the walk that honours the bits is
// `MountTable::resolve_at` (`crate::mount`).

use crate::types::Errno;

/// `RESOLVE_*` bits (include/uapi/linux/openat2.h).
pub const RESOLVE_NO_XDEV: u64 = 0x01;
pub const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
pub const RESOLVE_NO_SYMLINKS: u64 = 0x04;
pub const RESOLVE_BENEATH: u64 = 0x08;
pub const RESOLVE_IN_ROOT: u64 = 0x10;
pub const RESOLVE_CACHED: u64 = 0x20;

/// The bits Linux knows. Anything else is `EINVAL`.
const RESOLVE_KNOWN: u64 = 0x3f;
/// The bits this kernel implements. The other known ones are `EINVAL` too, for now
/// (`docs/reference/syscalls.md` lists them).
pub const RESOLVE_SUPPORTED: u64 = RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH;

/// `O_*` flags Linux accepts in `open_how.flags` (`VALID_OPEN_FLAGS`, x86-64 values).
const VALID_OPEN_FLAGS: u64 = 0o3 // O_ACCMODE
    | 0o100       // O_CREAT
    | 0o200       // O_EXCL
    | 0o400       // O_NOCTTY
    | 0o1000      // O_TRUNC
    | 0o2000      // O_APPEND
    | 0o4000      // O_NONBLOCK
    | 0o10000     // O_DSYNC
    | 0o20000     // FASYNC
    | 0o40000     // O_DIRECT
    | 0o100000    // O_LARGEFILE
    | 0o200000    // O_DIRECTORY
    | 0o400000    // O_NOFOLLOW
    | 0o1000000   // O_NOATIME
    | 0o2000000   // O_CLOEXEC
    | 0o4000000   // __O_SYNC
    | 0o10000000  // O_PATH
    | 0o20000000; // __O_TMPFILE

const O_CREAT: u64 = 0o100;
const O_TMPFILE: u64 = 0o20000000;

/// `sizeof(struct open_how)` in its first (and so far only) version.
pub const OPEN_HOW_SIZE_VER0: usize = 24;
/// Largest `size` accepted (`copy_struct_from_user` refuses more than a page).
pub const OPEN_HOW_SIZE_MAX: usize = 4096;

/// `struct open_how { u64 flags; u64 mode; u64 resolve; }`, validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenHow {
    pub flags: u64,
    pub mode: u64,
    pub resolve: u64,
}

impl OpenHow {
    /// Parse and validate the `size` bytes the caller passed (`bytes.len()` is `size`).
    ///
    /// - `EINVAL`: `size` smaller than version 0; unknown `flags` bits; a `mode` without
    ///   `O_CREAT`/`O_TMPFILE` or with bits above `07777`; unknown `resolve` bits;
    ///   `RESOLVE_BENEATH` together with `RESOLVE_IN_ROOT`; a known `resolve` bit this
    ///   kernel does not implement.
    /// - `E2BIG`: `size` above a page, or a larger struct whose extra bytes are not zero
    ///   (a newer caller asking for something this kernel cannot know).
    pub fn parse(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() > OPEN_HOW_SIZE_MAX {
            return Err(Errno::E2BIG);
        }
        if bytes.len() < OPEN_HOW_SIZE_VER0 {
            return Err(Errno::EINVAL);
        }
        if bytes[OPEN_HOW_SIZE_VER0..].iter().any(|&b| b != 0) {
            return Err(Errno::E2BIG);
        }
        let word = |i: usize| u64::from_ne_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap());
        let how = Self { flags: word(0), mode: word(1), resolve: word(2) };

        if how.flags & !VALID_OPEN_FLAGS != 0 {
            return Err(Errno::EINVAL);
        }
        if how.flags & (O_CREAT | O_TMPFILE) != 0 {
            if how.mode & !0o7777 != 0 {
                return Err(Errno::EINVAL);
            }
        } else if how.mode != 0 {
            return Err(Errno::EINVAL);
        }
        if how.resolve & !RESOLVE_KNOWN != 0 {
            return Err(Errno::EINVAL);
        }
        if how.resolve & RESOLVE_BENEATH != 0 && how.resolve & RESOLVE_IN_ROOT != 0 {
            return Err(Errno::EINVAL);
        }
        if how.resolve & !RESOLVE_SUPPORTED != 0 {
            return Err(Errno::EINVAL);
        }
        Ok(how)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn bytes(flags: u64, mode: u64, resolve: u64, extra: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&flags.to_ne_bytes());
        v.extend_from_slice(&mode.to_ne_bytes());
        v.extend_from_slice(&resolve.to_ne_bytes());
        v.extend_from_slice(extra);
        v
    }

    #[test]
    fn version_0_struct_parses() {
        let how = OpenHow::parse(&bytes(0o2, 0, RESOLVE_BENEATH, &[])).unwrap();
        assert_eq!(how, OpenHow { flags: 0o2, mode: 0, resolve: RESOLVE_BENEATH });
    }

    #[test]
    fn too_small_size_is_einval() {
        assert_eq!(OpenHow::parse(&bytes(0, 0, 0, &[])[..16]), Err(Errno::EINVAL));
        assert_eq!(OpenHow::parse(&[]), Err(Errno::EINVAL));
    }

    #[test]
    fn larger_struct_with_zero_tail_is_accepted() {
        assert!(OpenHow::parse(&bytes(0, 0, 0, &[0; 40])).is_ok());
    }

    #[test]
    fn larger_struct_with_nonzero_tail_is_e2big() {
        let mut tail = [0u8; 40];
        tail[39] = 1;
        assert_eq!(OpenHow::parse(&bytes(0, 0, 0, &tail)), Err(Errno::E2BIG));
    }

    #[test]
    fn size_above_a_page_is_e2big() {
        assert_eq!(OpenHow::parse(&bytes(0, 0, 0, &[0; 4096])), Err(Errno::E2BIG));
    }

    #[test]
    fn unknown_open_flag_is_einval() {
        assert_eq!(OpenHow::parse(&bytes(1 << 40, 0, 0, &[])), Err(Errno::EINVAL));
        assert_eq!(OpenHow::parse(&bytes(0o40000000, 0, 0, &[])), Err(Errno::EINVAL));
    }

    #[test]
    fn mode_rules() {
        // mode without O_CREAT/O_TMPFILE
        assert_eq!(OpenHow::parse(&bytes(0, 0o644, 0, &[])), Err(Errno::EINVAL));
        // mode with bits above 07777
        assert_eq!(OpenHow::parse(&bytes(O_CREAT, 0o10644, 0, &[])), Err(Errno::EINVAL));
        assert!(OpenHow::parse(&bytes(O_CREAT, 0o7777, 0, &[])).is_ok());
    }

    #[test]
    fn unknown_resolve_bit_is_einval() {
        assert_eq!(OpenHow::parse(&bytes(0, 0, 0x40, &[])), Err(Errno::EINVAL));
    }

    #[test]
    fn beneath_with_in_root_is_einval() {
        assert_eq!(OpenHow::parse(&bytes(0, 0, RESOLVE_BENEATH | RESOLVE_IN_ROOT, &[])), Err(Errno::EINVAL));
    }

    #[test]
    fn known_but_unimplemented_resolve_bits_are_einval() {
        for bit in [RESOLVE_NO_XDEV, RESOLVE_NO_MAGICLINKS, RESOLVE_IN_ROOT, RESOLVE_CACHED] {
            assert_eq!(OpenHow::parse(&bytes(0, 0, bit, &[])), Err(Errno::EINVAL), "bit {bit:#x}");
        }
        assert!(OpenHow::parse(&bytes(0, 0, RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH, &[])).is_ok());
    }
}
