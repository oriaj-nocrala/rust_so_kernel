//! A frame-pointer walk of a user stack, for the report the kernel prints
//! when it kills a process for a fault (`kernel/src/init/devices.rs`). The C
//! programs are built with `-fno-omit-frame-pointer`, so each frame starts
//! with `[saved rbp, return address]` at `rbp`.
//!
//! Defensive: the stack belongs to a program that just faulted. The walk
//! stops at a null, misaligned or unreadable `rbp`, at a chain that does not
//! move up the stack (a loop), after `MAX_FRAMES`, and at a jump of more
//! than `MAX_FRAME_BYTES` (garbage that happens to look like a pointer).

/// Frames reported, the faulting `rip` included.
pub const MAX_FRAMES: usize = 16;
/// A frame larger than this ends the walk.
pub const MAX_FRAME_BYTES: u64 = 1 << 20;

/// The return addresses, innermost first: `rip` itself, then one per frame.
/// Allocation-free (the fault path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frames {
    pub pcs: [u64; MAX_FRAMES],
    pub len: usize,
}

impl Frames {
    pub fn as_slice(&self) -> &[u64] {
        &self.pcs[..self.len]
    }
}

/// Walks from `rip`/`rbp`. `read(addr)` returns the u64 at a user address,
/// or `None` if it cannot be read.
pub fn walk(rip: u64, rbp: u64, mut read: impl FnMut(u64) -> Option<u64>) -> Frames {
    let mut f = Frames { pcs: [0; MAX_FRAMES], len: 0 };
    f.pcs[0] = rip;
    f.len = 1;
    let mut bp = rbp;
    while f.len < MAX_FRAMES {
        if bp == 0 || bp & 7 != 0 {
            break;
        }
        let (Some(next), Some(ret)) = (read(bp), read(bp.wrapping_add(8))) else { break };
        if ret == 0 {
            break;
        }
        f.pcs[f.len] = ret;
        f.len += 1;
        if next <= bp || next - bp > MAX_FRAME_BYTES {
            break;
        }
        bp = next;
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake stack: (address, value) pairs.
    fn reader(mem: &[(u64, u64)]) -> impl FnMut(u64) -> Option<u64> + '_ {
        move |a| mem.iter().find(|(k, _)| *k == a).map(|(_, v)| *v)
    }

    #[test]
    fn walks_a_chain_to_its_end() {
        // main's frame at 0x7000 (saved rbp 0 = outermost), f's at 0x6f00.
        let mem = [(0x6f00, 0x7000), (0x6f08, 0x401200), (0x7000, 0), (0x7008, 0x401050)];
        let f = walk(0x401111, 0x6f00, reader(&mem));
        assert_eq!(f.as_slice(), &[0x401111, 0x401200, 0x401050]);
    }

    #[test]
    fn stops_at_bad_frame_pointers() {
        let none = |_| None;
        assert_eq!(walk(0x401000, 0, none).as_slice(), &[0x401000]);
        assert_eq!(walk(0x401000, 0x6f03, |_| Some(1)).as_slice(), &[0x401000], "misaligned");
        assert_eq!(walk(0x401000, 0x6f00, none).as_slice(), &[0x401000], "unreadable");
    }

    #[test]
    fn a_loop_or_a_wild_jump_ends_the_walk() {
        let looped = [(0x6f00, 0x6f00), (0x6f08, 0x401200)];
        assert_eq!(walk(1, 0x6f00, reader(&looped)).as_slice(), &[1, 0x401200]);
        let down = [(0x6f00, 0x6e00), (0x6f08, 0x401200)];
        assert_eq!(walk(1, 0x6f00, reader(&down)).as_slice(), &[1, 0x401200]);
        let wild = [(0x6f00, 0x6f00 + MAX_FRAME_BYTES + 8), (0x6f08, 0x401200)];
        assert_eq!(walk(1, 0x6f00, reader(&wild)).as_slice(), &[1, 0x401200]);
    }

    #[test]
    fn at_most_max_frames() {
        // An endless well-formed chain: each frame 16 bytes above the last.
        let f = walk(1, 0x1000, |a| Some(if a % 16 == 0 { a + 16 } else { 0x400000 + a }));
        assert_eq!(f.len, MAX_FRAMES);
    }
}
