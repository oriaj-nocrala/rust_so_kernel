//! The kernel's random number generator: ChaCha20 with fast key erasure.
//!
//! Pure: the kernel gathers entropy (RDSEED/RDRAND, the TSC, the clock) and hands it to [`Rng::add_entropy`]; this module
//! turns it into bytes. The construction is the one Linux's `random.c` and OpenBSD's arc4random use: the key is a ChaCha20
//! key, every request is served from the keystream, and after each request the key is replaced with fresh keystream, so a
//! later compromise of the state does not reveal earlier output. The block function is RFC 8439 section 2.3.
//!
//! What it is *not*: a source of entropy. On a machine without RDSEED/RDRAND (QEMU's default CPU) the seed is only the TSC
//! and the clock, which is enough for hash-map keys and unpredictable-looking values, not for keys that must resist an
//! attacker who can guess the boot time (`docs/reference/syscalls.md`).

/// The four "expand 32-byte k" constants.
const SIGMA: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

fn quarter(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

/// One 64-byte ChaCha20 block (RFC 8439 section 2.3: 32-bit counter, 96-bit nonce), serialised little-endian.
pub fn chacha20_block(key: &[u32; 8], counter: u32, nonce: &[u32; 3]) -> [u8; 64] {
    let mut init = [0u32; 16];
    init[..4].copy_from_slice(&SIGMA);
    init[4..12].copy_from_slice(key);
    init[12] = counter;
    init[13..16].copy_from_slice(nonce);
    let mut s = init;
    for _ in 0..10 {
        quarter(&mut s, 0, 4, 8, 12);
        quarter(&mut s, 1, 5, 9, 13);
        quarter(&mut s, 2, 6, 10, 14);
        quarter(&mut s, 3, 7, 11, 15);
        quarter(&mut s, 0, 5, 10, 15);
        quarter(&mut s, 1, 6, 11, 12);
        quarter(&mut s, 2, 7, 8, 13);
        quarter(&mut s, 3, 4, 9, 14);
    }
    let mut out = [0u8; 64];
    for i in 0..16 {
        out[i * 4..i * 4 + 4].copy_from_slice(&s[i].wrapping_add(init[i]).to_le_bytes());
    }
    out
}

/// A block's first 32 bytes as a key.
fn key_from(block: &[u8; 64]) -> [u32; 8] {
    let mut k = [0u32; 8];
    for i in 0..8 {
        k[i] = u32::from_le_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
    }
    k
}

/// The most bytes one request may take before the key is replaced: 2^32 blocks would wrap the block counter.
pub const MAX_REQUEST: usize = 1 << 20;

#[derive(Clone)]
pub struct Rng {
    key: [u32; 8],
    /// Requests served (goes into the nonce, so two requests under the same key, before a reseed, never share a keystream).
    generation: u64,
    /// Bytes of entropy folded in so far (a diagnostic, not a promise).
    pub entropy_bytes: u64,
}

impl Rng {
    /// A generator whose key is `seed` folded into 32 bytes and whitened. An empty seed is allowed (the key is then a
    /// constant): call [`add_entropy`](Self::add_entropy) before trusting the output.
    pub fn new(seed: &[u8]) -> Rng {
        let mut r = Rng { key: [0; 8], generation: 0, entropy_bytes: 0 };
        r.add_entropy(seed);
        r
    }

    /// Fold `data` into the key (XOR into its 32 bytes, cycling) and re-derive the key from one block of the result, so
    /// every input bit can change every key bit.
    pub fn add_entropy(&mut self, data: &[u8]) {
        let mut bytes = [0u8; 32];
        for (i, w) in self.key.iter().enumerate() {
            bytes[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        for (i, b) in data.iter().enumerate() {
            bytes[i % 32] ^= *b;
        }
        let mut k = [0u32; 8];
        for i in 0..8 {
            k[i] = u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
        }
        // a nonce no request uses (requests count up from 0), and the length of the input so `[0]` and `[0, 0]` differ
        let nonce = [0xffff_ffff, 0xffff_ffff, data.len() as u32];
        self.key = key_from(&chacha20_block(&k, 0, &nonce));
        self.entropy_bytes += data.len() as u64;
    }

    /// Fill `out` with random bytes and replace the key (fast key erasure). Requests above [`MAX_REQUEST`] are split.
    pub fn fill(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(MAX_REQUEST) {
            let nonce = [self.generation as u32, (self.generation >> 32) as u32, 0];
            let mut counter = 1u32; // block 0 is the next key
            for piece in chunk.chunks_mut(64) {
                let block = chacha20_block(&self.key, counter, &nonce);
                piece.copy_from_slice(&block[..piece.len()]);
                counter += 1;
            }
            self.key = key_from(&chacha20_block(&self.key, 0, &nonce));
            self.generation += 1;
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.fill(&mut b);
        u64::from_le_bytes(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 8439 section 2.3.2: key 00 01 .. 1f, counter 1, nonce 00 00 00 09 00 00 00 4a 00 00 00 00.
    #[test]
    fn the_block_function_is_rfc_8439() {
        let mut key = [0u32; 8];
        for i in 0..8 {
            let b = |j: u32| (i as u32 * 4 + j) as u8;
            key[i] = u32::from_le_bytes([b(0), b(1), b(2), b(3)]);
        }
        let nonce = [0x0900_0000, 0x4a00_0000, 0];
        let want: [u8; 64] = [
            0x10, 0xf1, 0xe7, 0xe4, 0xd1, 0x3b, 0x59, 0x15, 0x50, 0x0f, 0xdd, 0x1f, 0xa3, 0x20, 0x71, 0xc4, 0xc7, 0xd1, 0xf4, 0xc7, 0x33, 0xc0, 0x68, 0x03,
            0x04, 0x22, 0xaa, 0x9a, 0xc3, 0xd4, 0x6c, 0x4e, 0xd2, 0x82, 0x64, 0x46, 0x07, 0x9f, 0xaa, 0x09, 0x14, 0xc2, 0xd7, 0x05, 0xd9, 0x8b, 0x02, 0xa2,
            0xb5, 0x12, 0x9c, 0xd1, 0xde, 0x16, 0x4e, 0xb9, 0xcb, 0xd0, 0x83, 0xe8, 0xa2, 0x50, 0x3c, 0x4e,
        ];
        assert_eq!(chacha20_block(&key, 1, &nonce), want);
    }

    #[test]
    fn a_quarter_round_is_rfc_8439() {
        // section 2.1.1
        let mut s = [0u32; 16];
        s[0] = 0x1111_1111;
        s[1] = 0x0102_0304;
        s[2] = 0x9b8d_6f43;
        s[3] = 0x0123_4567;
        quarter(&mut s, 0, 1, 2, 3);
        assert_eq!(&s[..4], &[0xea2a_92f4, 0xcb1c_f8ce, 0x4581_472e, 0x5881_c4bb]);
    }

    #[test]
    fn the_same_seed_gives_the_same_stream_and_another_seed_another() {
        let (mut a, mut b, mut c) = (Rng::new(b"seed one"), Rng::new(b"seed one"), Rng::new(b"seed two"));
        let (mut x, mut y, mut z) = ([0u8; 100], [0u8; 100], [0u8; 100]);
        a.fill(&mut x);
        b.fill(&mut y);
        c.fill(&mut z);
        assert_eq!(x, y);
        assert_ne!(x, z);
    }

    #[test]
    fn successive_requests_differ_and_the_key_is_erased() {
        let mut r = Rng::new(b"s");
        let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
        r.fill(&mut a);
        let key_after_a = r.key;
        r.fill(&mut b);
        assert_ne!(a, b);
        assert_ne!(r.key, key_after_a);
        // the output of a request is not derivable from the key that follows it: it is the keystream, the key is block 0
        let mut again = Rng::new(b"s");
        let mut a2 = [0u8; 32];
        again.fill(&mut a2);
        assert_eq!(a, a2);
        assert_eq!(again.key, key_after_a);
    }

    #[test]
    fn a_request_is_the_same_however_it_is_sliced() {
        // one request of 200 bytes = the keystream blocks 1..=4; two requests are two keystreams
        let mut r = Rng::new(b"s");
        let mut whole = [0u8; 200];
        r.fill(&mut whole);
        let mut odd = Rng::new(b"s");
        let mut part = [0u8; 200];
        odd.fill(&mut part);
        assert_eq!(whole, part);
        let mut two = Rng::new(b"s");
        let (mut p, mut q) = ([0u8; 100], [0u8; 100]);
        two.fill(&mut p);
        two.fill(&mut q);
        assert_eq!(&whole[..100], &p[..]);
        assert_ne!(&whole[100..], &q[..]);
    }

    #[test]
    fn entropy_changes_everything_after_it() {
        let mut a = Rng::new(b"s");
        let mut b = Rng::new(b"s");
        b.add_entropy(&[1]);
        let (mut x, mut y) = ([0u8; 16], [0u8; 16]);
        a.fill(&mut x);
        b.fill(&mut y);
        assert_ne!(x, y);
        // the length of the input matters, not only its bytes
        let (mut c, mut d) = (Rng::new(b"s"), Rng::new(b"s"));
        c.add_entropy(&[0]);
        d.add_entropy(&[0, 0]);
        assert_ne!(c.key, d.key);
        assert_eq!(b.entropy_bytes, 2);
    }

    #[test]
    fn the_output_never_contains_the_next_key() {
        // block 0 of the keystream becomes the next key: handing it out would give a caller the key of the next request
        let mut r = Rng::new(b"k");
        let mut out = [0u8; 64];
        r.fill(&mut out);
        let mut next = [0u8; 32];
        for (i, w) in r.key.iter().enumerate() {
            next[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        assert!(out.windows(32).all(|w| w != next));
    }

    #[test]
    fn entropy_accumulates_and_every_byte_of_it_counts() {
        // a second input is mixed into the first, not written over it, and order matters
        let (mut a, mut b, mut c) = (Rng::new(b"a"), Rng::new(b"b"), Rng::new(b"b"));
        a.add_entropy(b"b");
        c.add_entropy(b"a");
        assert_ne!(a.key, b.key);
        assert_ne!(a.key, c.key);
        // all 32 bytes of the key take part, the last one too
        let base = Rng::new(&[0u8; 32]).key;
        let mut last = [0u8; 32];
        last[31] = 1;
        assert_ne!(Rng::new(&last).key, base);
    }

    #[test]
    fn entropy_is_xored_into_the_key() {
        // feeding a generator its own key cancels it out: the derivation then starts from an all-zero key
        let mut r = Rng::new(&[]);
        let mut own = [0u8; 32];
        for (i, w) in r.key.iter().enumerate() {
            own[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        r.add_entropy(&own);
        let want = key_from(&chacha20_block(&[0; 8], 0, &[0xffff_ffff, 0xffff_ffff, 32]));
        assert_eq!(r.key, want);
    }

    #[test]
    fn a_big_request_is_served_in_chunks_with_a_new_key_each() {
        let mut r = Rng::new(b"big");
        let mut buf = vec![0u8; MAX_REQUEST + MAX_REQUEST / 2];
        r.fill(&mut buf);
        assert_eq!(r.generation, 2);
        assert_ne!(&buf[..64], &buf[MAX_REQUEST..MAX_REQUEST + 64]);
    }

    #[test]
    fn every_length_from_0_to_130_fills_exactly_its_slice() {
        let mut r = Rng::new(b"len");
        for n in 0..=130 {
            let mut buf = vec![0xa5u8; n + 2];
            r.fill(&mut buf[1..=n]);
            assert_eq!((buf[0], buf[n + 1]), (0xa5, 0xa5), "length {}", n);
        }
    }

    #[test]
    fn the_output_looks_uniform() {
        // not a statistical test suite: a stuck or biased generator (all zero, one byte value dominating) would show
        let mut r = Rng::new(b"uniform");
        let mut buf = vec![0u8; 1 << 16];
        r.fill(&mut buf);
        let mut hist = [0u32; 256];
        for b in &buf {
            hist[*b as usize] += 1;
        }
        // 256 expected per value; the 6-sigma band of a binomial is about 256 +- 96
        assert!(hist.iter().all(|&h| h > 150 && h < 370), "{:?}", hist);
        let ones: u32 = buf.iter().map(|b| b.count_ones()).sum();
        assert!((ones as i64 - (1 << 18)).abs() < 2000, "ones {}", ones);
    }
}
