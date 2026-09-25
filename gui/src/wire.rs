//! Wayland's wire format, unchanged.
//!
//! A message is `[object: u32][size << 16 | opcode: u32][args...]`, native
//! endian, `size` counting the whole message including the header and
//! always a multiple of 4. Arguments are 32-bit words: `int`, `uint`,
//! `object`, `new_id`, `fixed`; a `string` is a `u32` length including the
//! terminating NUL, the bytes, the NUL, then zero padding to 4 (a null
//! string is length 0). An `fd` argument takes **no bytes**: descriptors
//! travel out of band (`SCM_RIGHTS`) and are consumed in argument order.
//!
//! [`Decoder`] takes bytes and fds as they arrive — a read can end in the
//! middle of a message, and the fds of one `recvmsg` can belong to several
//! messages — and hands out whole messages.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

pub const HEADER: usize = 8;
/// Largest message accepted, as libwayland's `WL_MAX_MESSAGE_SIZE`.
pub const MAX_MESSAGE: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireError {
    /// A header whose size is below 8, not a multiple of 4, or above
    /// [`MAX_MESSAGE`]. The stream cannot be resynchronised after it.
    BadSize(u32),
    /// An argument runs past the end of its message.
    Truncated,
    /// A string without its NUL, or not UTF-8.
    BadString,
    /// A message wanted an fd and none had arrived.
    MissingFd,
    /// Bytes left over after the last argument.
    TrailingBytes,
}

/// Builds messages into a byte buffer plus the fds to send with it.
#[derive(Default)]
pub struct Encoder {
    pub bytes: Vec<u8>,
    pub fds: Vec<i32>,
    start: usize,
}

impl Encoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a message; finish it with [`Encoder::end`].
    pub fn begin(&mut self, object: u32, opcode: u16) -> &mut Self {
        self.start = self.bytes.len();
        self.put(object);
        self.put(opcode as u32); // size patched in by `end`
        self
    }

    fn put(&mut self, w: u32) {
        self.bytes.extend_from_slice(&w.to_ne_bytes());
    }

    pub fn uint(&mut self, v: u32) -> &mut Self {
        self.put(v);
        self
    }

    pub fn int(&mut self, v: i32) -> &mut Self {
        self.put(v as u32);
        self
    }

    pub fn string(&mut self, s: &str) -> &mut Self {
        self.put(s.len() as u32 + 1);
        self.bytes.extend_from_slice(s.as_bytes());
        self.bytes.push(0);
        while self.bytes.len() % 4 != 0 {
            self.bytes.push(0);
        }
        self
    }

    pub fn fd(&mut self, fd: i32) -> &mut Self {
        self.fds.push(fd);
        self
    }

    pub fn end(&mut self) {
        let size = (self.bytes.len() - self.start) as u32;
        let word = &mut self.bytes[self.start + 4..self.start + 8];
        let opcode = u32::from_ne_bytes([word[0], word[1], word[2], word[3]]);
        word.copy_from_slice(&(size << 16 | opcode).to_ne_bytes());
    }

    /// Takes everything encoded so far, leaving the encoder empty.
    pub fn take(&mut self) -> (Vec<u8>, Vec<i32>) {
        (core::mem::take(&mut self.bytes), core::mem::take(&mut self.fds))
    }
}

/// One whole message, arguments not yet interpreted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub object: u32,
    pub opcode: u16,
    pub body: Vec<u8>,
}

impl Message {
    pub fn args(&self) -> Args<'_> {
        Args { body: &self.body, pos: 0 }
    }
}

/// Reads a message's arguments in order.
pub struct Args<'a> {
    body: &'a [u8],
    pos: usize,
}

impl Args<'_> {
    pub fn uint(&mut self) -> Result<u32, WireError> {
        let b = self.body.get(self.pos..self.pos + 4).ok_or(WireError::Truncated)?;
        self.pos += 4;
        Ok(u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn int(&mut self) -> Result<i32, WireError> {
        self.uint().map(|v| v as i32)
    }

    /// A non-null string (a null one is `BadString`: nothing here takes one).
    pub fn string(&mut self) -> Result<String, WireError> {
        let len = self.uint()? as usize;
        if len == 0 {
            return Err(WireError::BadString);
        }
        let padded = (len + 3) & !3;
        let b = self.body.get(self.pos..self.pos + padded).ok_or(WireError::Truncated)?;
        self.pos += padded;
        if b[len - 1] != 0 {
            return Err(WireError::BadString);
        }
        core::str::from_utf8(&b[..len - 1]).map(String::from).map_err(|_| WireError::BadString)
    }

    /// Everything read: no stray bytes after the last argument.
    pub fn finish(&self) -> Result<(), WireError> {
        if self.pos == self.body.len() { Ok(()) } else { Err(WireError::TrailingBytes) }
    }
}

/// Reassembles messages from a byte stream and its out-of-band fds.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    fds: VecDeque<i32>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    pub fn push_fds(&mut self, fds: &[i32]) {
        self.fds.extend(fds.iter().copied());
    }

    /// The next fd, for the next `fd` argument of the message being read.
    pub fn take_fd(&mut self) -> Result<i32, WireError> {
        self.fds.pop_front().ok_or(WireError::MissingFd)
    }

    /// Fds received and never claimed by a message — for the caller to
    /// close when the client goes away.
    pub fn drain_fds(&mut self) -> Vec<i32> {
        self.fds.drain(..).collect()
    }

    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// The next whole message, `None` if it has not all arrived yet.
    pub fn next_message(&mut self) -> Result<Option<Message>, WireError> {
        if self.buf.len() < HEADER {
            return Ok(None);
        }
        let w = |i: usize| u32::from_ne_bytes([self.buf[i], self.buf[i + 1], self.buf[i + 2], self.buf[i + 3]]);
        let object = w(0);
        let word = w(4);
        let size = word >> 16;
        if (size as usize) < HEADER || size % 4 != 0 || size as usize > MAX_MESSAGE {
            return Err(WireError::BadSize(size));
        }
        if self.buf.len() < size as usize {
            return Ok(None);
        }
        let body = self.buf[HEADER..size as usize].to_vec();
        self.buf.drain(..size as usize);
        Ok(Some(Message { object, opcode: (word & 0xFFFF) as u16, body }))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;

    fn sample() -> (Vec<u8>, Vec<i32>) {
        let mut e = Encoder::new();
        e.begin(1, 0).uint(7).fd(42).int(-5).end();
        e.begin(9, 4).string("héllo").end();
        e.begin(3, 2).end();
        e.take()
    }

    #[test]
    fn header_and_string_layout_are_waylands() {
        let mut e = Encoder::new();
        e.begin(5, 3).string("abc").end();
        let (b, _) = e.take();
        // 8 header + 4 length + "abc\0" (already aligned) = 16.
        assert_eq!(b.len(), 16);
        assert_eq!(u32::from_ne_bytes(b[4..8].try_into().unwrap()), 16 << 16 | 3);
        assert_eq!(u32::from_ne_bytes(b[8..12].try_into().unwrap()), 4);
        assert_eq!(&b[12..16], b"abc\0");
        e.begin(5, 3).string("abcd").end();
        assert_eq!(e.take().0.len(), 8 + 4 + 8); // "abcd\0" padded to 8
    }

    #[test]
    fn roundtrip_with_every_split_point() {
        let (bytes, fds) = sample();
        for cut in 0..=bytes.len() {
            let mut d = Decoder::new();
            d.push_bytes(&bytes[..cut]);
            let mut got = vec![];
            while let Some(m) = d.next_message().unwrap() {
                got.push(m);
            }
            d.push_bytes(&bytes[cut..]);
            d.push_fds(&fds);
            while let Some(m) = d.next_message().unwrap() {
                got.push(m);
            }
            assert_eq!(got.len(), 3, "cut at {cut}");
            let mut a = got[0].args();
            assert_eq!((got[0].object, got[0].opcode), (1, 0));
            assert_eq!(a.uint().unwrap(), 7);
            assert_eq!(d.take_fd().unwrap(), 42);
            assert_eq!(a.int().unwrap(), -5);
            a.finish().unwrap();
            let mut a = got[1].args();
            assert_eq!(a.string().unwrap(), "héllo");
            a.finish().unwrap();
            assert!(got[2].body.is_empty());
            assert_eq!(d.buffered(), 0);
        }
    }

    #[test]
    fn bad_sizes_are_errors() {
        for size in [0u32, 4, 10, (MAX_MESSAGE + 4) as u32] {
            let mut d = Decoder::new();
            d.push_bytes(&1u32.to_ne_bytes());
            d.push_bytes(&(size << 16).to_ne_bytes());
            assert_eq!(d.next_message(), Err(WireError::BadSize(size)));
        }
    }

    #[test]
    fn malformed_arguments() {
        let m = |body: Vec<u8>| Message { object: 1, opcode: 0, body };
        assert_eq!(m(vec![1, 2]).args().uint(), Err(WireError::Truncated));
        // Length says 8 but only 4 bytes follow.
        let mut b = 8u32.to_ne_bytes().to_vec();
        b.extend_from_slice(b"abc\0");
        assert_eq!(m(b).args().string(), Err(WireError::Truncated));
        // No NUL where the length puts it.
        let mut b = 4u32.to_ne_bytes().to_vec();
        b.extend_from_slice(b"abcd");
        assert_eq!(m(b).args().string(), Err(WireError::BadString));
        // Null string.
        assert_eq!(m(0u32.to_ne_bytes().to_vec()).args().string(), Err(WireError::BadString));
        // Trailing bytes.
        let msg = m(vec![0; 8]);
        let mut a = msg.args();
        a.uint().unwrap();
        assert_eq!(a.finish(), Err(WireError::TrailingBytes));
        let mut d = Decoder::new();
        assert_eq!(d.take_fd(), Err(WireError::MissingFd));
    }
}
