use core::marker::PhantomData;

// Atributos de una entrada de la IDT
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct IdtEntryOptions(u16);

impl IdtEntryOptions {
    const PRESENT: u16 = 1 << 15;
    const INTERRUPT_GATE: u16 = 0xE << 8;
    const TRAP_GATE: u16 = 0xF << 8;

    pub fn new() -> Self {
        // Por defecto, creamos una entrada presente y de tipo interrupt gate
        IdtEntryOptions(Self::PRESENT | Self::INTERRUPT_GATE)
    }

    pub fn set_present(mut self, present: bool) -> Self {
        self.0 = (self.0 & !Self::PRESENT) | (if present { Self::PRESENT } else { 0 });
        self
    }

    pub fn set_privilege_level(mut self, dpl: u16) -> Self {
        self.0 = (self.0 & !0x6000) | ((dpl & 0b11) << 13);
        self
    }
}

// Entrada en la Tabla de Descriptores de Interrupciones (IDT)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
#[repr(packed)]
pub struct IdtEntry<F> {
    pointer_low: u16,
    gdt_selector: u16,
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
        // TODO: Cargar el selector del GDT de forma dinámica
        self.gdt_selector = 8; // Asumimos un selector de código de 8 por ahora
        self.options = self.options.set_present(true);
        self
    }
}

pub type HandlerFunc = extern "x86-interrupt" fn();

// La IDT. Es un array de 256 entradas.
#[derive(Debug)]
#[repr(C)]
pub struct InterruptDescriptorTable {
    entries: [IdtEntry<HandlerFunc>; 256],
}

impl InterruptDescriptorTable {
    pub fn new() -> Self {
        InterruptDescriptorTable {
            entries: [IdtEntry::missing(); 256],
        }
    }

    pub fn add_handler(&mut self, vector: u8, handler: HandlerFunc) {
        self.entries[vector as usize]
            .set_handler_addr(handler as u64);
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

// Estructura que se pasa a la instrucción `lidt`
#[repr(C, packed(2))]
struct IdtDescriptor {
    size: u16,
    address: u64,
}