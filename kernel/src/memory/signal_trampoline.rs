// kernel/src/memory/signal_trampoline.rs
//
// The fixed sigreturn trampoline: one read+exec page, identical bytes in
// every user address space, mapped by `elf_loader::load_elf`.
//
// Lives here (not in `process::signal`) to respect this kernel's dependency
// rule that `memory` never imports `process` (see CLAUDE.md) — `elf_loader`
// needs these constants to map the page, and `process::signal` needs the
// same address to know where a caught signal's return-to-kernel lands;
// `process` already depends on `memory` everywhere else, so this direction
// is the only one that keeps the layering intact.

/// Fixed user virtual address of the trampoline page.
///
/// Must land inside one of the three PML4 slots `OwnedPageTable::new_user`
/// actually reserves for user mappings — 0 (code), 128 (mmap region,
/// `USER_MMAP_BASE`), or 226 (stack, `elf_loader::DEFAULT_STACK_BASE`); any
/// other PML4 index gets a kernel-copied (non-user) entry and `map_user_page`
/// fails there. Placed near the top of PML4[128]'s ~512 GiB range — mmap's
/// bump allocator starts at `USER_MMAP_BASE` and grows upward, so this is
/// unreachable by any realistic mmap usage.
pub const TRAMPOLINE_VA: u64 = 0x0000_407F_FFFF_F000;

/// `mov eax, SYS_SIGRETURN ; syscall` — SYS_SIGRETURN must match
/// `process::syscall::SyscallNumber::Sigreturn`'s discriminant (15).
pub const TRAMPOLINE_CODE: [u8; 7] = [0xB8, 15, 0, 0, 0, 0x0F, 0x05];
