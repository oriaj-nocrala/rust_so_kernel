//! The receive queue shared by both socket types.
//!
//! A socket's queue holds what its *peer* has written to it, as a list of
//! segments. One segment is one `send()`:
//!
//! - **`SOCK_DGRAM`** keeps segments whole — one segment in, one segment out,
//!   message boundaries preserved, a read that doesn't fit truncates and
//!   discards the rest (`MSG_TRUNC`).
//! - **`SOCK_STREAM`** treats them as one continuous byte stream: a read
//!   walks as many segments as it needs, and a write coalesces into the last
//!   segment instead of adding another.
//!
//! The reason a *stream* needs segments at all is `SCM_RIGHTS`. File
//! descriptors sent alongside data are attached to the byte they were sent
//! with, and must be handed to the reader when it reaches that byte — not
//! earlier, not later. A segment is exactly that association, so a send
//! carrying fds always starts a new one, and a stream read never crosses into
//! a following segment that carries fds of its own (Linux's
//! `unix_stream_read_generic` breaks out of its skb loop on the same
//! condition). Without this the queue could be a flat ring like `pipe.rs`'s.
//!
//! `F` is the file-descriptor payload type. The kernel instantiates it as
//! `Box<dyn FileHandle>`; tests here use plain integers. Nothing in this
//! module interprets an `F` — it only has to be carried and eventually handed
//! back, which is what makes the fd-passing logic host-testable at all. Same
//! genericity trick as `sched::SchedCore<Process>`.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::addr::UnixAddr;

/// One `send()`: its bytes, the fds attached to it, and (for datagrams) the
/// sender's address.
struct Segment<F> {
    data: Vec<u8>,
    /// Bytes already consumed off the front (stream reads only).
    off: usize,
    fds: Vec<F>,
    from: UnixAddr,
}

impl<F> Segment<F> {
    fn remaining(&self) -> usize {
        self.data.len() - self.off
    }
}

/// The outcome of a datagram read.
pub struct DgramRead<F> {
    /// Bytes actually copied into the caller's buffer.
    pub n: usize,
    /// The datagram's real length — larger than `n` when it was truncated.
    pub full_len: usize,
    pub fds: Vec<F>,
    pub from: UnixAddr,
}

pub struct RecvQueue<F> {
    segs: VecDeque<Segment<F>>,
    bytes: usize,
    cap: usize,
}

impl<F> RecvQueue<F> {
    pub fn new(cap: usize) -> Self {
        Self { segs: VecDeque::new(), bytes: 0, cap }
    }

    pub fn is_empty(&self) -> bool {
        self.segs.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Room left, in bytes.
    pub fn space(&self) -> usize {
        self.cap.saturating_sub(self.bytes)
    }

    /// Resize the queue (`SO_RCVBUF`). Never drops already-queued data, so
    /// the queue can legitimately sit over capacity until it drains.
    pub fn set_capacity(&mut self, cap: usize) {
        self.cap = cap;
    }

    /// Append to the stream, coalescing into the last segment when neither
    /// side carries fds. Returns how many bytes were accepted (0 when full),
    /// plus the fds back if nothing was accepted — an unsent `SCM_RIGHTS`
    /// must not be silently swallowed.
    pub fn push_stream(&mut self, data: &[u8], fds: Vec<F>) -> (usize, Vec<F>) {
        let n = data.len().min(self.space());
        if n == 0 {
            return (0, fds);
        }

        let coalesce = fds.is_empty()
            && self.segs.back().map(|s| s.fds.is_empty()).unwrap_or(false);

        if coalesce {
            // `back_mut` is Some whenever `coalesce` is true.
            if let Some(last) = self.segs.back_mut() {
                last.data.extend_from_slice(&data[..n]);
            }
        } else {
            self.segs.push_back(Segment {
                data: Vec::from(&data[..n]),
                off: 0,
                fds,
                from: UnixAddr::Unnamed,
            });
        }
        self.bytes += n;
        (n, Vec::new())
    }

    /// Append one whole datagram, or nothing at all. Returns the fds back on
    /// refusal, for the same reason `push_stream` does.
    pub fn push_dgram(&mut self, data: &[u8], fds: Vec<F>, from: UnixAddr) -> Result<(), Vec<F>> {
        // A zero-length datagram is a real, deliverable message — it must
        // still be refused when the queue is full, so test space, not length.
        if data.len() > self.space() {
            return Err(fds);
        }
        self.segs.push_back(Segment {
            data: Vec::from(data),
            off: 0,
            fds,
            from,
        });
        self.bytes += data.len();
        Ok(())
    }

    /// Read bytes off the stream, walking segments until `buf` is full.
    ///
    /// **A read never crosses a segment boundary that either side of carries
    /// fds.** The bytes a `SCM_RIGHTS` was sent with are delivered as one
    /// unit: the read stops when it finishes an fd-carrying segment, and
    /// stops before entering one if it has already copied something. Two
    /// batches of descriptors therefore never arrive from a single read, and
    /// a reader can always tell which bytes its descriptors came with —
    /// Linux's `unix_stream_read_generic` refuses to glue such skbs together
    /// for the same reason (`unix_skb_scm_eq`).
    ///
    /// `peek` copies without consuming, and deliberately hands back **no**
    /// fds: installing a descriptor in the reader is not something a peek can
    /// undo, so a peeked read that also passed fds would duplicate them.
    pub fn read_stream(&mut self, buf: &mut [u8], peek: bool) -> (usize, Vec<F>) {
        let mut done = 0usize;
        let mut fds: Vec<F> = Vec::new();
        let mut peek_seg = 0usize;
        let mut peek_off = 0usize;

        while done < buf.len() {
            let (has_fds, remaining) = if peek {
                match self.segs.get(peek_seg) {
                    Some(s) => (!s.fds.is_empty(), s.data.len() - s.off - peek_off),
                    None => break,
                }
            } else {
                match self.segs.front() {
                    Some(s) => (!s.fds.is_empty(), s.remaining()),
                    None => break,
                }
            };

            // Don't cross into a second fd-carrying segment.
            if has_fds && done > 0 {
                break;
            }
            if remaining == 0 {
                if peek {
                    peek_seg += 1;
                    peek_off = 0;
                    continue;
                }
                // A fully consumed segment is popped below; this only happens
                // for an empty stream segment, which nothing pushes.
                self.segs.pop_front();
                continue;
            }

            let n = remaining.min(buf.len() - done);
            if peek {
                let seg = &self.segs[peek_seg];
                let start = seg.off + peek_off;
                buf[done..done + n].copy_from_slice(&seg.data[start..start + n]);
                peek_off += n;
                if seg.off + peek_off >= seg.data.len() {
                    peek_seg += 1;
                    peek_off = 0;
                }
            } else {
                {
                    let seg = self.segs.front_mut().expect("checked above");
                    buf[done..done + n].copy_from_slice(&seg.data[seg.off..seg.off + n]);
                    seg.off += n;
                    if !seg.fds.is_empty() {
                        fds.append(&mut seg.fds);
                    }
                }
                self.bytes -= n;
                if self.segs.front().map(|s| s.remaining() == 0).unwrap_or(false) {
                    self.segs.pop_front();
                }
            }
            done += n;

            // Finished an fd-carrying segment: stop rather than gluing the
            // next writer's bytes onto this batch's.
            if has_fds && n == remaining {
                break;
            }
        }

        (done, fds)
    }

    /// Take one datagram, truncating it to `buf` and discarding the rest.
    pub fn read_dgram(&mut self, buf: &mut [u8], peek: bool) -> Option<DgramRead<F>> {
        if peek {
            let seg = self.segs.front()?;
            let n = seg.data.len().min(buf.len());
            buf[..n].copy_from_slice(&seg.data[..n]);
            return Some(DgramRead {
                n,
                full_len: seg.data.len(),
                fds: Vec::new(), // see read_stream's note on peeking fds
                from: seg.from.clone(),
            });
        }

        let mut seg = self.segs.pop_front()?;
        self.bytes -= seg.data.len();
        let n = seg.data.len().min(buf.len());
        buf[..n].copy_from_slice(&seg.data[..n]);
        Some(DgramRead {
            n,
            full_len: seg.data.len(),
            fds: core::mem::take(&mut seg.fds),
            from: seg.from,
        })
    }

    /// The size of the datagram at the head, for `FIONREAD`-style checks.
    pub fn peek_dgram_len(&self) -> Option<usize> {
        self.segs.front().map(|s| s.data.len())
    }

    /// Drop everything, handing back every fd still in flight so the caller
    /// can close them. An undelivered `SCM_RIGHTS` that is merely dropped
    /// leaks an open file description for the lifetime of the system.
    pub fn drain_fds(&mut self) -> Vec<F> {
        let mut fds = Vec::new();
        for mut seg in self.segs.drain(..) {
            fds.append(&mut seg.fds);
        }
        self.bytes = 0;
        fds
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn q() -> RecvQueue<u32> {
        RecvQueue::new(16)
    }

    #[test]
    fn stream_writes_coalesce_and_read_back_as_one_run() {
        let mut q = q();
        assert_eq!(q.push_stream(b"abc", vec![]).0, 3);
        assert_eq!(q.push_stream(b"def", vec![]).0, 3);

        let mut buf = [0u8; 8];
        let (n, fds) = q.read_stream(&mut buf, false);
        assert_eq!(&buf[..n], b"abcdef");
        assert!(fds.is_empty());
        assert!(q.is_empty());
    }

    #[test]
    fn stream_write_is_truncated_to_available_space() {
        let mut q = RecvQueue::<u32>::new(4);
        assert_eq!(q.push_stream(b"abcdef", vec![]).0, 4);
        assert_eq!(q.space(), 0);
        let (n, returned) = q.push_stream(b"g", vec![7]);
        assert_eq!(n, 0);
        assert_eq!(returned, vec![7], "unsent fds must come back to the sender");
    }

    #[test]
    fn stream_partial_read_leaves_the_rest_queued() {
        let mut q = q();
        q.push_stream(b"abcdef", vec![]);
        let mut buf = [0u8; 2];
        assert_eq!(q.read_stream(&mut buf, false).0, 2);
        assert_eq!(&buf, b"ab");
        assert_eq!(q.bytes(), 4);
        let mut rest = [0u8; 8];
        let (n, _) = q.read_stream(&mut rest, false);
        assert_eq!(&rest[..n], b"cdef");
    }

    #[test]
    fn stream_fds_arrive_with_the_bytes_they_were_sent_with() {
        let mut q = q();
        q.push_stream(b"aa", vec![]);
        q.push_stream(b"bb", vec![42]);

        // First read gets the leading fd-less bytes and no fds...
        let mut buf = [0u8; 2];
        let (n, fds) = q.read_stream(&mut buf, false);
        assert_eq!((&buf[..n], fds.as_slice()), (&b"aa"[..], &[][..]));

        // ...the next read reaches the fd-carrying segment.
        let (n, fds) = q.read_stream(&mut buf, false);
        assert_eq!((&buf[..n], fds.as_slice()), (&b"bb"[..], &[42][..]));
    }

    #[test]
    fn a_stream_read_never_merges_two_fd_batches() {
        let mut q = q();
        q.push_stream(b"a", vec![1]);
        q.push_stream(b"b", vec![2]);

        let mut buf = [0u8; 8];
        let (n, fds) = q.read_stream(&mut buf, false);
        assert_eq!((n, fds), (1, vec![1]), "stopped at the second fd segment");
        let (n, fds) = q.read_stream(&mut buf, false);
        assert_eq!((n, fds), (1, vec![2]));
    }

    #[test]
    fn stream_peek_does_not_consume_and_yields_no_fds() {
        let mut q = q();
        q.push_stream(b"ab", vec![9]);
        q.push_stream(b"cd", vec![]);

        // A peek shows exactly what the next real read will return — same
        // stop-at-the-fd-segment rule — minus the descriptors themselves.
        let mut buf = [0u8; 8];
        let (n, fds) = q.read_stream(&mut buf, true);
        assert_eq!(&buf[..n], b"ab");
        assert!(fds.is_empty(), "peek must not install descriptors");
        assert_eq!(q.bytes(), 4, "peek must not consume");

        let (n, fds) = q.read_stream(&mut buf, false);
        assert_eq!((&buf[..n], fds), (&b"ab"[..], vec![9]));
        let (n, fds) = q.read_stream(&mut buf, false);
        assert_eq!((&buf[..n], fds), (&b"cd"[..], vec![]));
    }

    #[test]
    fn a_read_stops_after_an_fd_segment_instead_of_gluing_the_next_bytes_on() {
        let mut q = q();
        q.push_stream(b"ab", vec![9]);
        q.push_stream(b"cd", vec![]);

        let mut buf = [0u8; 8];
        let (n, fds) = q.read_stream(&mut buf, false);
        assert_eq!(
            (&buf[..n], fds),
            (&b"ab"[..], vec![9]),
            "the reader must be able to tell which bytes its fds came with"
        );
    }

    #[test]
    fn a_partial_read_of_an_fd_segment_keeps_the_rest_readable() {
        let mut q = q();
        q.push_stream(b"abcd", vec![9]);
        let mut buf = [0u8; 2];
        let (n, fds) = q.read_stream(&mut buf, false);
        assert_eq!((&buf[..n], fds), (&b"ab"[..], vec![9]), "fds arrive with the first bytes");
        let (n, fds) = q.read_stream(&mut buf, false);
        assert_eq!((&buf[..n], fds), (&b"cd"[..], vec![]));
    }

    #[test]
    fn dgram_preserves_message_boundaries() {
        let mut q = q();
        q.push_dgram(b"one", vec![], UnixAddr::Unnamed).unwrap();
        q.push_dgram(b"two", vec![], UnixAddr::Unnamed).unwrap();

        let mut buf = [0u8; 8];
        let r = q.read_dgram(&mut buf, false).unwrap();
        assert_eq!(&buf[..r.n], b"one");
        let r = q.read_dgram(&mut buf, false).unwrap();
        assert_eq!(&buf[..r.n], b"two");
        assert!(q.read_dgram(&mut buf, false).is_none());
    }

    #[test]
    fn dgram_read_truncates_and_reports_the_full_length() {
        let mut q = q();
        q.push_dgram(b"abcdef", vec![], UnixAddr::Unnamed).unwrap();
        let mut buf = [0u8; 2];
        let r = q.read_dgram(&mut buf, false).unwrap();
        assert_eq!((r.n, r.full_len), (2, 6));
        assert!(q.is_empty(), "the remainder of a truncated datagram is discarded");
    }

    #[test]
    fn a_zero_length_datagram_is_a_real_message() {
        let mut q = q();
        q.push_dgram(b"", vec![], UnixAddr::Unnamed).unwrap();
        let mut buf = [0u8; 4];
        let r = q.read_dgram(&mut buf, false).unwrap();
        assert_eq!((r.n, r.full_len), (0, 0));
    }

    #[test]
    fn dgram_that_does_not_fit_is_refused_whole() {
        let mut q = RecvQueue::<u32>::new(4);
        assert_eq!(q.push_dgram(b"abcde", vec![3], UnixAddr::Unnamed), Err(vec![3]));
        assert!(q.is_empty());
    }

    #[test]
    fn dgram_carries_the_senders_address() {
        let mut q = q();
        let from = UnixAddr::Abstract(vec![b'x']);
        q.push_dgram(b"hi", vec![], from.clone()).unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(q.read_dgram(&mut buf, false).unwrap().from, from);
    }

    #[test]
    fn draining_hands_back_every_fd_still_in_flight() {
        let mut q = q();
        q.push_stream(b"a", vec![1, 2]);
        q.push_stream(b"b", vec![3]);
        let mut fds = q.drain_fds();
        fds.sort();
        assert_eq!(fds, vec![1, 2, 3]);
        assert!(q.is_empty());
        assert_eq!(q.bytes(), 0);
    }
}
