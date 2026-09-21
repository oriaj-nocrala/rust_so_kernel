//! `struct sockaddr_un` — the wire format, parsed and produced as data.
//!
//! The layout is Linux's, byte for byte:
//!
//! ```c
//! struct sockaddr_un {
//!     sa_family_t sun_family;   /* u16, AF_UNIX == 1 */
//!     char        sun_path[108];
//! };
//! ```
//!
//! `sun_family` is a **16-bit** field. This kernel's own `abi-bits/socket.h`
//! used to typedef `sa_family_t` as `unsigned int`, which silently shifted
//! `sun_path` two bytes down and made every address this port produced
//! incompatible with the real ABI — the same class of drift CLAUDE.md's
//! "ABI-constant hygiene" note records for `SEEK_SET`/`O_CREAT`/`MAP_ANONYMOUS`.
//! Parsing lives here, in plain data, so that layout is pinned by host tests
//! instead of by whatever the header happens to say.
//!
//! Three address shapes exist, exactly as in Linux:
//!
//! - **Unnamed** (`addrlen == 2`): no name at all. A socket that was never
//!   bound, and what `getsockname()` reports for one.
//! - **Pathname**: `sun_path` holds a NUL-terminated filesystem path. The
//!   name lives in the filesystem — `bind()` creates a socket node there.
//! - **Abstract** (`sun_path[0] == '\0'`): a Linux extension. The name is the
//!   remaining `addrlen - 3` bytes *verbatim*, embedded NULs included, and
//!   lives in a kernel-private namespace with no filesystem presence at all.
//!   That is why the name is `Vec<u8>` and not `String`: it is not text.

use alloc::string::String;
use alloc::vec::Vec;

use crate::SockError;

/// `AF_UNIX` / `AF_LOCAL`, the real Linux value.
pub const AF_UNIX: u16 = 1;

/// `sizeof(((struct sockaddr_un *)0)->sun_path)`.
pub const SUN_PATH_LEN: usize = 108;

/// `sizeof(struct sockaddr_un)` — the largest `addrlen` that can be valid.
pub const SOCKADDR_UN_LEN: usize = 2 + SUN_PATH_LEN;

/// A parsed AF_UNIX address.
///
/// `Ord` is derived purely so the bind registry can be a `BTreeMap` keyed by
/// address; the ordering itself carries no meaning.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum UnixAddr {
    /// Never bound — `addrlen == sizeof(sa_family_t)`.
    Unnamed,
    /// A filesystem path (`sun_path` up to its first NUL).
    Path(String),
    /// Linux's abstract namespace: `sun_path[0] == '\0'`, name is the rest.
    Abstract(Vec<u8>),
}

impl UnixAddr {
    /// Parse the first `addrlen` bytes of a user-supplied `struct sockaddr_un`.
    ///
    /// `bytes` must be exactly the `addrlen` the caller passed — the length is
    /// what distinguishes an unnamed address from a zero-length abstract one,
    /// and what bounds an abstract name (which may contain NULs).
    pub fn parse(bytes: &[u8]) -> Result<Self, SockError> {
        if bytes.len() < 2 || bytes.len() > SOCKADDR_UN_LEN {
            return Err(SockError::Inval);
        }
        let family = u16::from_le_bytes([bytes[0], bytes[1]]);
        if family != AF_UNIX {
            return Err(SockError::AfNoSupport);
        }

        let path = &bytes[2..];
        if path.is_empty() {
            return Ok(Self::Unnamed);
        }
        if path[0] == 0 {
            // Abstract: the name is every remaining byte, verbatim.
            return Ok(Self::Abstract(path[1..].to_vec()));
        }

        // Pathname: NUL-terminated, or running to the end of addrlen.
        let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
        let name = core::str::from_utf8(&path[..end]).map_err(|_| SockError::Inval)?;
        Ok(Self::Path(String::from(name)))
    }

    /// The `addrlen` this address occupies on the wire.
    pub fn encoded_len(&self) -> usize {
        match self {
            Self::Unnamed => 2,
            // Pathname addresses include the terminating NUL, as Linux reports.
            Self::Path(p) => 2 + p.len() + 1,
            Self::Abstract(n) => 2 + 1 + n.len(),
        }
    }

    /// Write this address into `out` in `sockaddr_un` form.
    ///
    /// Returns the address's *full* length even when `out` was too short to
    /// hold it — `getsockname`/`getpeername`/`recvfrom` report that length
    /// back to userspace so a caller can detect truncation, exactly as Linux
    /// does.
    pub fn encode(&self, out: &mut [u8]) -> usize {
        let full = self.encoded_len();
        let mut buf = [0u8; SOCKADDR_UN_LEN];
        buf[0..2].copy_from_slice(&AF_UNIX.to_le_bytes());
        match self {
            Self::Unnamed => {}
            Self::Path(p) => {
                let n = p.len().min(SUN_PATH_LEN - 1);
                buf[2..2 + n].copy_from_slice(&p.as_bytes()[..n]);
                // buf is zeroed, so the terminating NUL is already there.
            }
            Self::Abstract(name) => {
                let n = name.len().min(SUN_PATH_LEN - 1);
                buf[3..3 + n].copy_from_slice(&name[..n]);
            }
        }
        let copy = full.min(out.len()).min(SOCKADDR_UN_LEN);
        out[..copy].copy_from_slice(&buf[..copy]);
        full
    }

    /// True for an address that can actually be bound to or connected to.
    pub fn is_named(&self) -> bool {
        !matches!(self, Self::Unnamed)
    }

    /// The filesystem path, for the one caller that has to create/unlink a
    /// node for it (the kernel adapter's `bind`).
    pub fn as_path(&self) -> Option<&str> {
        match self {
            Self::Path(p) => Some(p.as_str()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn sockaddr(family: u16, path: &[u8]) -> Vec<u8> {
        let mut v = Vec::from(&family.to_le_bytes()[..]);
        v.extend_from_slice(path);
        v
    }

    #[test]
    fn family_field_is_two_bytes_not_four() {
        // The whole point of pinning the layout: sun_path starts at offset 2.
        let raw = sockaddr(AF_UNIX, b"/tmp/s\0");
        assert_eq!(UnixAddr::parse(&raw), Ok(UnixAddr::Path(String::from("/tmp/s"))));

        // With a 4-byte family field the same bytes would parse as a path
        // starting mid-string ("mp/s"); assert that is NOT what happens.
        assert_ne!(UnixAddr::parse(&raw), Ok(UnixAddr::Path(String::from("mp/s"))));
    }

    #[test]
    fn addrlen_two_is_unnamed() {
        assert_eq!(UnixAddr::parse(&sockaddr(AF_UNIX, b"")), Ok(UnixAddr::Unnamed));
    }

    #[test]
    fn abstract_name_keeps_embedded_nuls_and_is_bounded_by_addrlen() {
        let raw = sockaddr(AF_UNIX, b"\0ab\0cd");
        assert_eq!(
            UnixAddr::parse(&raw),
            Ok(UnixAddr::Abstract(vec![b'a', b'b', 0, b'c', b'd']))
        );

        // Same bytes, shorter addrlen: the name is genuinely shorter.
        assert_eq!(
            UnixAddr::parse(&raw[..2 + 3]),
            Ok(UnixAddr::Abstract(vec![b'a', b'b']))
        );
    }

    #[test]
    fn abstract_with_empty_name_is_distinct_from_unnamed() {
        assert_eq!(
            UnixAddr::parse(&sockaddr(AF_UNIX, b"\0")),
            Ok(UnixAddr::Abstract(Vec::new()))
        );
    }

    #[test]
    fn pathname_stops_at_first_nul_even_with_trailing_garbage() {
        let raw = sockaddr(AF_UNIX, b"/tmp/s\0junk");
        assert_eq!(UnixAddr::parse(&raw), Ok(UnixAddr::Path(String::from("/tmp/s"))));
    }

    #[test]
    fn unterminated_pathname_runs_to_addrlen() {
        let raw = sockaddr(AF_UNIX, b"/tmp/s");
        assert_eq!(UnixAddr::parse(&raw), Ok(UnixAddr::Path(String::from("/tmp/s"))));
    }

    #[test]
    fn wrong_family_and_bad_lengths_are_rejected() {
        assert_eq!(UnixAddr::parse(&sockaddr(2 /* AF_INET */, b"x")), Err(SockError::AfNoSupport));
        assert_eq!(UnixAddr::parse(&[1u8]), Err(SockError::Inval));
        assert_eq!(UnixAddr::parse(&vec![0u8; SOCKADDR_UN_LEN + 1]), Err(SockError::Inval));
    }

    #[test]
    fn encode_round_trips_every_shape() {
        for addr in [
            UnixAddr::Unnamed,
            UnixAddr::Path(String::from("/tmp/sock")),
            UnixAddr::Abstract(vec![b'x', 0, b'y']),
        ] {
            let mut out = [0u8; SOCKADDR_UN_LEN];
            let n = addr.encode(&mut out);
            assert_eq!(n, addr.encoded_len());
            assert_eq!(UnixAddr::parse(&out[..n]), Ok(addr));
        }
    }

    #[test]
    fn encode_into_short_buffer_reports_full_length() {
        let addr = UnixAddr::Path(String::from("/tmp/sock"));
        let mut out = [0u8; 5];
        assert_eq!(addr.encode(&mut out), addr.encoded_len());
        assert_eq!(&out[..2], &AF_UNIX.to_le_bytes()[..]);
        assert_eq!(&out[2..5], b"/tm");
    }

    #[test]
    fn encoded_pathname_length_counts_the_terminating_nul() {
        assert_eq!(UnixAddr::Path(String::from("/a")).encoded_len(), 2 + 2 + 1);
        assert_eq!(UnixAddr::Abstract(vec![b'a', b'b']).encoded_len(), 2 + 1 + 2);
    }
}
