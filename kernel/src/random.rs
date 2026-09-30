// kernel/src/random.rs
//
// The kernel's random number generator: `getrandom(318)`, `/dev/urandom`, `/dev/random`. The construction (ChaCha20 with fast
// key erasure) is `hal::random`, host-tested against RFC 8439; this file gathers the entropy and owns the global.
//
// Entropy: RDSEED and RDRAND when CPUID says they exist (the Ryzen has both; QEMU's default CPU has neither), the TSC, the
// clock, and the jitter of a few TSC deltas. Every request also folds in the TSC and a fresh RDRAND word, so an unseeded
// start (no hardware source) is at least never the same twice. Not a substitute for entropy on a machine without a hardware
// source: see `hal::random`.
//
// Locking: taken from process context only (never from an ISR), so a plain `crate::sync::Mutex`.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use ::hal::random::Rng;

use crate::sync::Mutex;

static RNG: Mutex<Option<Rng>> = Mutex::new(None);
/// Which sources fed the seed: bit 0 RDSEED, bit 1 RDRAND (for `/proc/kdebug`).
static SOURCES: AtomicU32 = AtomicU32::new(0);
static REQUESTS: AtomicU64 = AtomicU64::new(0);

const SRC_RDSEED: u32 = 1;
const SRC_RDRAND: u32 = 2;

fn cpu_has_rdrand() -> bool {
    // CPUID.1:ECX[30]
    core::arch::x86_64::__cpuid(1).ecx & (1 << 30) != 0
}

fn cpu_has_rdseed() -> bool {
    // CPUID.(7,0):EBX[18]
    core::arch::x86_64::__cpuid(0).eax >= 7 && core::arch::x86_64::__cpuid_count(7, 0).ebx & (1 << 18) != 0
}

/// One word from `rdrand`/`rdseed`, retried a few times (both may report "not ready").
fn hw_word(seed: bool) -> Option<u64> {
    for _ in 0..16 {
        let (v, ok): (u64, u8);
        // SAFETY: the caller checked CPUID for the instruction; both only write their outputs.
        unsafe {
            if seed {
                core::arch::asm!("rdseed {v}", "setc {ok}", v = out(reg) v, ok = out(reg_byte) ok, options(nomem, nostack));
            } else {
                core::arch::asm!("rdrand {v}", "setc {ok}", v = out(reg) v, ok = out(reg_byte) ok, options(nomem, nostack));
            }
        }
        if ok != 0 {
            return Some(v);
        }
    }
    None
}

/// The seed: what the hardware gives plus timing. Returns the bytes and the source bits.
fn gather() -> ([u8; 128], u32) {
    let mut buf = [0u8; 128];
    let mut at = 0;
    let mut put = |v: u64, buf: &mut [u8; 128]| {
        if at + 8 <= buf.len() {
            buf[at..at + 8].copy_from_slice(&v.to_le_bytes());
            at += 8;
        }
    };
    let mut sources = 0;
    if cpu_has_rdseed() {
        for _ in 0..4 {
            if let Some(v) = hw_word(true) {
                put(v, &mut buf);
                sources |= SRC_RDSEED;
            }
        }
    }
    if cpu_has_rdrand() {
        for _ in 0..4 {
            if let Some(v) = hw_word(false) {
                put(v, &mut buf);
                sources |= SRC_RDRAND;
            }
        }
    }
    put(crate::cpu::tsc::read(), &mut buf);
    put(crate::time::ktime_get(), &mut buf);
    // jitter: how long a short dependent loop takes, read from the TSC, a few times over
    for _ in 0..8 {
        let t0 = crate::cpu::tsc::read();
        let mut x = t0;
        for _ in 0..64 {
            x = x.wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(13);
        }
        core::hint::black_box(x);
        put(crate::cpu::tsc::read().wrapping_sub(t0), &mut buf);
    }
    (buf, sources)
}

fn seeded(slot: &mut Option<Rng>) -> &mut Rng {
    slot.get_or_insert_with(|| {
        let (seed, sources) = gather();
        SOURCES.store(sources, Ordering::Relaxed);
        Rng::new(&seed)
    })
}

/// Fill `out` with random bytes. Seeds the generator on first use.
pub fn fill(out: &mut [u8]) {
    let mut guard = RNG.lock();
    let rng = seeded(&mut guard);
    // a request is never served from the state a previous one left alone: fold in the TSC and, if there is one, a hardware word
    let mut mix = [0u8; 16];
    mix[..8].copy_from_slice(&crate::cpu::tsc::read().to_le_bytes());
    if SOURCES.load(Ordering::Relaxed) & SRC_RDRAND != 0 {
        if let Some(v) = hw_word(false) {
            mix[8..].copy_from_slice(&v.to_le_bytes());
        }
    }
    rng.add_entropy(&mix);
    rng.fill(out);
    REQUESTS.fetch_add(1, Ordering::Relaxed);
}

/// Mix caller-supplied bytes into the generator (a write to `/dev/urandom`); no entropy is credited.
pub fn add_entropy(data: &[u8]) {
    let mut guard = RNG.lock();
    seeded(&mut guard).add_entropy(data);
}

/// `/proc/kdebug` line.
pub fn render_kdebug() -> alloc::string::String {
    let s = SOURCES.load(Ordering::Relaxed);
    alloc::format!(
        "random: seeded={} rdseed={} rdrand={} requests={}",
        RNG.lock().is_some() as u32,
        (s & SRC_RDSEED != 0) as u32,
        (s & SRC_RDRAND != 0) as u32,
        REQUESTS.load(Ordering::Relaxed)
    )
}
