//! Who has to drop a stale translation — the pure half of the TLB shootdown,
//! stage 5 of `docs/smp/smp-plan.md`.
//!
//! After a page-table change the CPU that made it invalidates its own TLB,
//! and every *other* CPU that can hold the old entry has to be told to do the
//! same (an IPI) and waited for. Telling too few is silent memory corruption:
//! the forgotten CPU keeps reading and writing a frame that now belongs to
//! someone else. Telling too many only costs an interrupt. So the rule is
//! conservative and small enough to test on its own:
//!
//! * A **kernel** mapping lives in every address space and may be GLOBAL
//!   (survives a CR3 switch): every CPU that is up can hold it.
//! * A **user** mapping belongs to one page table. Without PCIDs (this
//!   kernel uses none) a CR3 write drops every non-global entry, so only the
//!   CPUs whose CR3 *is* that table right now can hold it.
//!
//! "Is up" is `ready`: a CPU enters it only once it can take the IPI (IDT and
//! LAPIC live) and after flushing its whole TLB, so nothing it cached before
//! joining can be missed. A CPU never in `ready` — not started, or parked
//! after a failed init — is never waited for.
//!
//! CPU sets are `u32` bitmasks: `MAX_CPUS` is 32.

/// Which translations changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// A user mapping in the page table whose PML4 is at this physical
    /// address.
    AddressSpace(u64),
    /// A kernel mapping (physical window, MMIO, framebuffer, guard pages).
    Kernel,
}

/// The physical-address bits of a CR3 value: PCD/PWT (bits 3, 4) and the
/// rest of the low 12 bits are flags, not part of the table's address.
pub const fn cr3_table(cr3: u64) -> u64 {
    cr3 & 0x000F_FFFF_FFFF_F000
}

/// The CPUs other than `me` that must be sent a shootdown for `scope`.
/// `loaded[c]` is the page table CPU `c` has loaded (see [`cr3_table`]);
/// `ready` the CPUs that take part at all.
pub fn targets(scope: Scope, loaded: &[u64], ready: u32, me: usize) -> u32 {
    let mut mask = 0u32;
    for (cpu, &cr3) in loaded.iter().enumerate().take(32) {
        let bit = 1u32 << cpu;
        if cpu == me || ready & bit == 0 {
            continue;
        }
        let holds = match scope {
            Scope::Kernel => true,
            Scope::AddressSpace(pml4) => cr3_table(cr3) == cr3_table(pml4),
        };
        if holds {
            mask |= bit;
        }
    }
    mask
}

/// Does the CPU whose CR3 is `my_cr3` hold translations for `scope`?
pub fn holds(scope: Scope, my_cr3: u64) -> bool {
    match scope {
        Scope::Kernel => true,
        Scope::AddressSpace(pml4) => cr3_table(my_cr3) == cr3_table(pml4),
    }
}

/// The CPU numbers in `mask`, lowest first.
pub fn cpus(mut mask: u32) -> impl Iterator<Item = usize> {
    core::iter::from_fn(move || {
        if mask == 0 {
            return None;
        }
        let cpu = mask.trailing_zeros() as usize;
        mask &= mask - 1;
        Some(cpu)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    const KERNEL: u64 = 0x1000;
    const A: u64 = 0x5_0000;
    const B: u64 = 0x7_0000;

    #[test]
    fn kernel_scope_reaches_every_ready_cpu_but_me() {
        let loaded = [KERNEL, A, B, KERNEL];
        assert_eq!(targets(Scope::Kernel, &loaded, 0b1111, 0), 0b1110);
        assert_eq!(targets(Scope::Kernel, &loaded, 0b1111, 2), 0b1011);
    }

    #[test]
    fn address_space_scope_reaches_only_cpus_running_it() {
        let loaded = [A, A, B, KERNEL, A];
        assert_eq!(targets(Scope::AddressSpace(A), &loaded, 0b11111, 0), 0b10010);
        assert_eq!(targets(Scope::AddressSpace(B), &loaded, 0b11111, 0), 0b00100);
        // Nobody runs it (a fork child being built, say): nobody to tell.
        assert_eq!(targets(Scope::AddressSpace(0x9_0000), &loaded, 0b11111, 0), 0);
    }

    #[test]
    fn a_cpu_not_ready_is_never_waited_for() {
        // CPU 1 runs A but is not (or no longer) taking IPIs: waiting for it
        // would hang the sender.
        let loaded = [A, A, A];
        assert_eq!(targets(Scope::AddressSpace(A), &loaded, 0b101, 0), 0b100);
        assert_eq!(targets(Scope::Kernel, &loaded, 0b001, 0), 0);
    }

    #[test]
    fn cr3_flag_bits_do_not_hide_a_match() {
        // CR3 with PWT|PCD set names the same table.
        let loaded = [KERNEL, A | 0x18];
        assert_eq!(targets(Scope::AddressSpace(A), &loaded, 0b11, 0), 0b10);
        assert!(holds(Scope::AddressSpace(A), A | 0x18));
        assert!(!holds(Scope::AddressSpace(A), B));
        assert!(holds(Scope::Kernel, B));
    }

    #[test]
    fn all_32_cpus_fit() {
        let loaded = [A; 32];
        assert_eq!(targets(Scope::Kernel, &loaded, u32::MAX, 31), u32::MAX >> 1);
        assert_eq!(targets(Scope::AddressSpace(A), &loaded, u32::MAX, 0), u32::MAX - 1);
        assert_eq!(cpus(u32::MAX).count(), 32);
    }

    #[test]
    fn cpus_lists_the_mask_in_order() {
        assert_eq!(cpus(0b1010_0101).collect::<Vec<_>>(), [0, 2, 5, 7]);
        assert_eq!(cpus(0).count(), 0);
        assert_eq!(cpus(1 << 31).collect::<Vec<_>>(), [31]);
    }
}
