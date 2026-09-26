//! Decoding `cpuid` results into what `/proc/cpuinfo` prints.
//!
//! Pure: the kernel executes `cpuid` and hands the registers here. The
//! field layout is the one both vendors share (Intel SDM vol. 2A "CPUID",
//! AMD APM vol. 3 appendix E); the flag names are Linux's
//! (`arch/x86/include/asm/cpufeatures.h`), in Linux's word order, so a
//! program that greps `/proc/cpuinfo` for `sse4_2` or `avx2` finds them.

/// One `cpuid` result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Regs {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

/// Leaf 0's vendor string: EBX, EDX, ECX, in that order
/// ("Genu" "ineI" "ntel").
pub fn vendor(leaf0: Regs) -> [u8; 12] {
    let mut v = [0u8; 12];
    v[0..4].copy_from_slice(&leaf0.ebx.to_le_bytes());
    v[4..8].copy_from_slice(&leaf0.edx.to_le_bytes());
    v[8..12].copy_from_slice(&leaf0.ecx.to_le_bytes());
    v
}

/// Family, model and stepping as Linux prints them (decimal, extended
/// fields folded in).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signature {
    pub family: u32,
    pub model: u32,
    pub stepping: u32,
}

/// Leaf 1's EAX. The extended family is added only when the base family is
/// 0xF, the extended model is prepended only for families 6 and 0xF — the
/// rule both manuals give (and Linux's `x86_family`/`x86_model`).
pub fn signature(eax: u32) -> Signature {
    let stepping = eax & 0xF;
    let base_model = (eax >> 4) & 0xF;
    let base_family = (eax >> 8) & 0xF;
    let ext_model = (eax >> 16) & 0xF;
    let ext_family = (eax >> 20) & 0xFF;
    let family = if base_family == 0xF { base_family + ext_family } else { base_family };
    let model = if base_family == 0x6 || base_family == 0xF { (ext_model << 4) | base_model } else { base_model };
    Signature { family, model, stepping }
}

/// Leaves 0x8000_0002..=0x8000_0004: the 48-byte brand string, EAX EBX
/// ECX EDX of each in turn.
pub fn brand(leaves: [Regs; 3]) -> [u8; 48] {
    let mut b = [0u8; 48];
    for (i, r) in leaves.iter().enumerate() {
        for (j, v) in [r.eax, r.ebx, r.ecx, r.edx].iter().enumerate() {
            b[i * 16 + j * 4..][..4].copy_from_slice(&v.to_le_bytes());
        }
    }
    b
}

/// The printable part of a brand or vendor string: up to the first NUL,
/// surrounding spaces removed (Intel pads its brand string on the left).
/// `None` if what is left is empty or not ASCII.
pub fn trimmed(bytes: &[u8]) -> Option<&str> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let s = core::str::from_utf8(&bytes[..end]).ok()?.trim_matches(' ');
    (!s.is_empty() && s.is_ascii()).then_some(s)
}

/// The registers flags are read from. Leaves that the CPU does not have
/// (max leaf below 6 or 7, max extended leaf below 0x8000_0001) are passed
/// as zero.
#[derive(Clone, Copy, Debug, Default)]
pub struct FeatureRegs {
    pub l1_edx: u32,
    pub l1_ecx: u32,
    pub l7_ebx: u32,
    pub l7_ecx: u32,
    /// Leaf 6 (thermal and power management): only `aperfmperf`, bit 0.
    pub l6_ecx: u32,
    pub e1_edx: u32,
    pub e1_ecx: u32,
}

#[derive(Clone, Copy)]
enum Word {
    L1Edx,
    E1Edx,
    L1Ecx,
    E1Ecx,
    L6Ecx,
    L7Ebx,
    L7Ecx,
}

/// (register, bit, Linux name), in Linux's word order: 1.EDX, 8000_0001.EDX,
/// 1.ECX, 8000_0001.ECX, then Linux's word 7 (bits it gathers from other
/// leaves — here 6.ECX's `aperfmperf`), 7.EBX, 7.ECX. A subset: the flags programs
/// actually test for, not every bit either manual defines.
const FLAGS: &[(Word, u8, &str)] = &[
    (Word::L1Edx, 0, "fpu"), (Word::L1Edx, 1, "vme"), (Word::L1Edx, 2, "de"),
    (Word::L1Edx, 3, "pse"), (Word::L1Edx, 4, "tsc"), (Word::L1Edx, 5, "msr"),
    (Word::L1Edx, 6, "pae"), (Word::L1Edx, 7, "mce"), (Word::L1Edx, 8, "cx8"),
    (Word::L1Edx, 9, "apic"), (Word::L1Edx, 11, "sep"), (Word::L1Edx, 12, "mtrr"),
    (Word::L1Edx, 13, "pge"), (Word::L1Edx, 14, "mca"), (Word::L1Edx, 15, "cmov"),
    (Word::L1Edx, 16, "pat"), (Word::L1Edx, 17, "pse36"), (Word::L1Edx, 19, "clflush"),
    (Word::L1Edx, 23, "mmx"), (Word::L1Edx, 24, "fxsr"), (Word::L1Edx, 25, "sse"),
    (Word::L1Edx, 26, "sse2"), (Word::L1Edx, 28, "ht"),
    (Word::E1Edx, 11, "syscall"), (Word::E1Edx, 20, "nx"), (Word::E1Edx, 22, "mmxext"),
    (Word::E1Edx, 25, "fxsr_opt"), (Word::E1Edx, 26, "pdpe1gb"), (Word::E1Edx, 27, "rdtscp"),
    (Word::E1Edx, 29, "lm"),
    (Word::L1Ecx, 0, "pni"), (Word::L1Ecx, 1, "pclmulqdq"), (Word::L1Ecx, 3, "monitor"),
    (Word::L1Ecx, 9, "ssse3"), (Word::L1Ecx, 12, "fma"), (Word::L1Ecx, 13, "cx16"),
    (Word::L1Ecx, 19, "sse4_1"), (Word::L1Ecx, 20, "sse4_2"), (Word::L1Ecx, 21, "x2apic"),
    (Word::L1Ecx, 22, "movbe"), (Word::L1Ecx, 23, "popcnt"), (Word::L1Ecx, 25, "aes"),
    (Word::L1Ecx, 26, "xsave"), (Word::L1Ecx, 28, "avx"), (Word::L1Ecx, 29, "f16c"),
    (Word::L1Ecx, 30, "rdrand"), (Word::L1Ecx, 31, "hypervisor"),
    (Word::E1Ecx, 0, "lahf_lm"), (Word::E1Ecx, 2, "svm"), (Word::E1Ecx, 5, "abm"),
    (Word::E1Ecx, 6, "sse4a"), (Word::E1Ecx, 8, "3dnowprefetch"),
    (Word::L6Ecx, 0, "aperfmperf"),
    (Word::L7Ebx, 0, "fsgsbase"), (Word::L7Ebx, 3, "bmi1"), (Word::L7Ebx, 5, "avx2"),
    (Word::L7Ebx, 7, "smep"), (Word::L7Ebx, 8, "bmi2"), (Word::L7Ebx, 9, "erms"),
    (Word::L7Ebx, 16, "avx512f"), (Word::L7Ebx, 18, "rdseed"), (Word::L7Ebx, 19, "adx"),
    (Word::L7Ebx, 20, "smap"), (Word::L7Ebx, 23, "clflushopt"), (Word::L7Ebx, 29, "sha_ni"),
    (Word::L7Ecx, 2, "umip"), (Word::L7Ecx, 9, "vaes"), (Word::L7Ecx, 10, "vpclmulqdq"),
    (Word::L7Ecx, 22, "rdpid"),
];

/// Every flag set in `r`, in Linux's order.
pub fn flags(r: &FeatureRegs) -> impl Iterator<Item = &'static str> + '_ {
    FLAGS.iter().filter_map(move |&(word, bit, name)| {
        let v = match word {
            Word::L1Edx => r.l1_edx,
            Word::E1Edx => r.e1_edx,
            Word::L1Ecx => r.l1_ecx,
            Word::E1Ecx => r.e1_ecx,
            Word::L6Ecx => r.l6_ecx,
            Word::L7Ebx => r.l7_ebx,
            Word::L7Ecx => r.l7_ecx,
        };
        (v & (1 << bit) != 0).then_some(name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regs(s: &[u8; 16]) -> Regs {
        let w = |i: usize| u32::from_le_bytes(s[i * 4..][..4].try_into().unwrap());
        Regs { eax: w(0), ebx: w(1), ecx: w(2), edx: w(3) }
    }

    #[test]
    fn vendor_is_ebx_edx_ecx() {
        let r = Regs {
            eax: 0x10,
            ebx: u32::from_le_bytes(*b"Auth"),
            edx: u32::from_le_bytes(*b"enti"),
            ecx: u32::from_le_bytes(*b"cAMD"),
        };
        assert_eq!(&vendor(r), b"AuthenticAMD");
    }

    #[test]
    fn ryzen_5900x_signature() {
        // Zen 3 Vermeer, B0: family 0xF + ext 0xA = 25, model 0x21 = 33.
        assert_eq!(signature(0x00A2_0F10), Signature { family: 25, model: 33, stepping: 0 });
    }

    #[test]
    fn intel_family_6_takes_the_extended_model_but_not_the_extended_family() {
        // Coffee Lake i7-8700: 06_9EH, stepping 10.
        assert_eq!(signature(0x0009_06EA), Signature { family: 6, model: 158, stepping: 10 });
        // A family-5 part ignores both extended fields even if set.
        assert_eq!(signature(0x00F5_0543), Signature { family: 5, model: 4, stepping: 3 });
    }

    #[test]
    fn brand_is_assembled_and_trimmed() {
        let b = brand([
            regs(b"      Intel(R) C"),
            regs(b"ore(TM) i7-8700 "),
            regs(b"CPU @ 3.20GHz\0\0\0"),
        ]);
        assert_eq!(trimmed(&b), Some("Intel(R) Core(TM) i7-8700 CPU @ 3.20GHz"));
        assert_eq!(trimmed(&[0u8; 48]), None);
        assert_eq!(trimmed(b"    \0junk"), None);
    }

    #[test]
    fn flags_come_out_in_linux_order_and_only_when_set() {
        let r = FeatureRegs {
            l1_edx: 1 << 0 | 1 << 26,
            l1_ecx: 1 << 20 | 1 << 31,
            l7_ebx: 1 << 5,
            l6_ecx: 1 << 0 | 1 << 3,
            e1_edx: 1 << 29,
            ..Default::default()
        };
        let got: std::vec::Vec<&str> = flags(&r).collect();
        assert_eq!(got, ["fpu", "sse2", "lm", "sse4_2", "hypervisor", "aperfmperf", "avx2"]);
        assert_eq!(flags(&FeatureRegs::default()).count(), 0);
    }

    #[test]
    fn no_flag_name_or_bit_is_listed_twice() {
        for (i, a) in FLAGS.iter().enumerate() {
            for b in &FLAGS[i + 1..] {
                assert_ne!(a.2, b.2);
                assert!(!(a.0 as u8 == b.0 as u8 && a.1 == b.1), "{} and {} share a bit", a.2, b.2);
            }
        }
    }
}
