// kernel/src/interrupts/idt.rs
//
// Interrupt Descriptor Table
//
// UPDATED: Added IST (Interrupt Stack Table) support.
//   - `IdtEntry::set_ist_index(index)` sets bits 0:2 of the IST field.
//   - `InterruptDescriptorTable::add_double_fault_handler` now accepts
//     an optional IST index.
//   - This ensures double faults use a dedicated stack, preventing
//     triple faults on stack overflow.

use core::marker::PhantomData;
use crate::interrupts::exception::ExceptionStackFrame;

// ============================================================================
// IDT Entry Options
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct IdtEntryOptions(u16);

impl IdtEntryOptions {
    const PRESENT: u16 = 1 << 15;
    const INTERRUPT_GATE: u16 = 0xE << 8;
    const TRAP_GATE: u16 = 0xF << 8;

    pub fn interrupt_gate() -> Self {
        IdtEntryOptions(Self::PRESENT | Self::INTERRUPT_GATE)
    }

    pub fn trap_gate() -> Self {
        IdtEntryOptions(Self::PRESENT | Self::TRAP_GATE)
    }

    #[allow(dead_code)]
    pub fn set_privilege_level(mut self, dpl: u16) -> Self {
        self.0 = (self.0 & !0x6000) | ((dpl & 0b11) << 13);
        self
    }
}

// ============================================================================
// IDT Entry
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
#[repr(packed)]
pub struct IdtEntry<F> {
    pointer_low: u16,
    gdt_selector: u16,
    /// Bits 0:2 = IST index (0 = don't use IST, 1-7 = IST entry).
    /// Bits 8:11 = gate type.
    /// Bit 15 = present.
    /// Bits 13:14 = DPL.
    options: IdtEntryOptions,
    pointer_middle: u16,
    pointer_high: u32,
    reserved: u32,
    phantom: PhantomData<F>,
}

impl<F> IdtEntry<F> {
    pub fn missing() -> Self {
        IdtEntry {
            gdt_selector: 0,
            pointer_low: 0,
            pointer_middle: 0,
            pointer_high: 0,
            options: IdtEntryOptions(0),
            reserved: 0,
            phantom: PhantomData,
        }
    }

    pub fn set_handler_addr(&mut self, addr: u64) -> &mut Self {
        self.pointer_low = addr as u16;
        self.pointer_middle = (addr >> 16) as u16;
        self.pointer_high = (addr >> 32) as u32;
        self.gdt_selector = 8; // Kernel code segment
        self.options = IdtEntryOptions::interrupt_gate();
        self
    }

    pub fn set_privilege_level(&mut self, dpl: u16) -> &mut Self {
        self.options.0 = (self.options.0 & !0x6000) | ((dpl & 0b11) << 13);
        self
    }

    /// Set the IST (Interrupt Stack Table) index for this entry.
    ///
    /// `index` is 1-based (1..=7).  The CPU will switch to
    /// `TSS.interrupt_stack_table[index - 1]` before invoking the handler.
    /// This is critical for double faults: without a separate stack,
    /// a stack overflow causes a triple fault (immediate reset).
    ///
    /// The IST index occupies bits 0:2 of the byte at offset 4 in the
    /// IDT entry.  In our packed repr, that's the low 3 bits of `options`.
    /// However, the x86-64 IDT entry format has the IST field in the
    /// "IST" byte which is separate from the type/DPL/P bits.
    ///
    /// Actually, in the 16-byte IDT entry layout:
    ///   Bytes 0-1: offset low
    ///   Bytes 2-3: segment selector
    ///   Byte  4:   bits 0:2 = IST, bits 3:7 = reserved (must be 0)
    ///   Byte  5:   bits 0:3 = gate type, bit 4 = 0, bits 5:6 = DPL, bit 7 = P
    ///   Bytes 6-7: offset middle
    ///   Bytes 8-11: offset high
    ///   Bytes 12-15: reserved
    ///
    /// Our `options` field is a u16 covering bytes 4-5.
    /// IST is in the LOW byte (byte 4), bits 0:2.
    /// Gate type, DPL, P are in the HIGH byte (byte 5).
    pub fn set_ist_index(&mut self, index: u16) -> &mut Self {
        debug_assert!(index <= 7, "IST index must be 0-7, got {}", index);
        // Clear old IST bits (low 3 bits of the u16) and set new ones
        self.options.0 = (self.options.0 & !0x07) | (index & 0x07);
        self
    }
}

// ============================================================================
// Handler types
// ============================================================================

pub type ExceptionHandler = extern "x86-interrupt" fn(&mut ExceptionStackFrame);
pub type ExceptionHandlerWithErrCode = extern "x86-interrupt" fn(&mut ExceptionStackFrame, error_code: u64);
pub type DoubleFaultHandler = extern "x86-interrupt" fn(&mut ExceptionStackFrame, error_code: u64) -> !;

// ============================================================================
// Interrupt Descriptor Table
// ============================================================================

#[derive(Debug)]
#[repr(C)]
pub struct InterruptDescriptorTable {
    pub entries: [IdtEntry<ExceptionHandler>; 256],
}

impl InterruptDescriptorTable {
    pub fn new() -> Self {
        InterruptDescriptorTable {
            entries: [IdtEntry::missing(); 256],
        }
    }

    pub fn add_handler(&mut self, vector: u8, handler: ExceptionHandler) {
        self.entries[vector as usize]
            .set_handler_addr(handler as u64);
    }

    pub fn add_handler_with_error(&mut self, vector: u8, handler: ExceptionHandlerWithErrCode) {
        self.entries[vector as usize]
            .set_handler_addr(handler as u64);
    }

    /// Register a double fault handler with an IST index.
    ///
    /// The IST index ensures the CPU switches to a known-good stack
    /// before invoking the handler.  This prevents triple faults when
    /// the original stack is corrupted (e.g. stack overflow).
    ///
    /// `ist_index` is 1-based (1..=7), matching TSS.interrupt_stack_table
    /// indices (0-based internally, but the CPU uses 1-based in the IDT).
    pub fn add_double_fault_handler(
        &mut self,
        vector: u8,
        handler: DoubleFaultHandler,
        ist_index: u16,
    ) {
        self.entries[vector as usize]
            .set_handler_addr(handler as u64);
        self.entries[vector as usize]
            .set_ist_index(ist_index);
    }

    pub fn load(&'static self) {
        use core::mem::size_of;
        let descriptor = IdtDescriptor {
            size: (size_of::<Self>() - 1) as u16,
            address: self as *const _ as u64,
        };
        unsafe {
            core::arch::asm!("lidt [{}]", in(reg) &descriptor, options(nostack));
        }
    }
}

// ============================================================================
// LIDT descriptor
// ============================================================================

#[repr(C, packed(2))]
struct IdtDescriptor {
    size: u16,
    address: u64,
}