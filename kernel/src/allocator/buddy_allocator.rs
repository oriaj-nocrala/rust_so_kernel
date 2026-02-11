// kernel/src/allocator/buddy_allocator.rs

use x86_64::PhysAddr;
use spin::Mutex;

const MIN_ORDER: usize = 12; // 4KB (2^12)
const MAX_ORDER: usize = 28; // 256MB (2^28) - ajustable según tu RAM

pub struct BuddyAllocator {
    free_lists: [FreeList; MAX_ORDER - MIN_ORDER + 1],
    total_memory: u64,
}

#[derive(Clone, Copy)]
struct FreeList {
    head: Option<PhysAddr>,
}

impl FreeList {
    const fn new() -> Self {
        Self { head: None }
    }
}

/// Metadata almacenada en cada bloque libre
#[repr(C)]
struct FreeBlock {
    next: Option<PhysAddr>,
}

impl BuddyAllocator {
    pub const fn new() -> Self {
        const INIT: FreeList = FreeList::new();
        Self {
            free_lists: [INIT; MAX_ORDER - MIN_ORDER + 1],
            total_memory: 0,
        }
    }

    /// Convierte order absoluto (12-20) a índice de array (0-8)
    #[inline]
    fn order_to_index(&self, order: usize) -> usize {
        order - MIN_ORDER
    }

    /// Agrega una región de memoria al buddy allocator
    pub unsafe fn add_region(&mut self, start: u64, end: u64) {
        // let safe_start = start.max(0);
        
        // if safe_start >= end {
        //     // ❌ NO usar serial_println! aquí
        //     // crate::serial_print_raw!("Buddy: Skipping region (too low)\n");
        //     return;
        // }
        
        let mut current_addr = start;
        let region_size = end - start;
        
        // ❌ NO usar serial_println! aquí tampoco
        // crate::serial_print_raw!("Buddy: Adding region\n");

        self.total_memory += region_size;

        while current_addr < end {
            let remaining = end - current_addr;
            
            if remaining < (1 << MIN_ORDER) {
                break;
            }

            let align_order = current_addr.trailing_zeros() as usize;
            let size_order = (63 - remaining.leading_zeros()) as usize;
            
            let order = align_order
                .min(size_order)
                .min(MAX_ORDER)
                .max(MIN_ORDER);

            let block_size = 1u64 << order;
            
            // ❌ NO usar serial_println! aquí
            // crate::serial_print_raw!("  Adding block\n");

            self.add_block(order, PhysAddr::new(current_addr));
            current_addr += block_size;
        }
        
        // crate::serial_print_raw!("Buddy: Region added\n");
    }

    /// Agrega un bloque a la lista libre de su order
    unsafe fn add_block(&mut self, order: usize, addr: PhysAddr) {
        let idx = self.order_to_index(order);
        let phys_offset = crate::memory::physical_memory_offset();
        let virt_addr = phys_offset + addr.as_u64();

        // Crear el bloque que apunta al head actual
        let new_block = FreeBlock {
            next: self.free_lists[idx].head,
        };

        // Escribir en la dirección física (via virtual mapping)
        let ptr = virt_addr.as_mut_ptr::<FreeBlock>();
        ptr.write(new_block);

        // Actualizar head
        self.free_lists[idx].head = Some(addr);
    }

    /// Remueve un bloque específico de su lista
    unsafe fn remove_block(&mut self, order: usize, addr: PhysAddr) {
        let idx = self.order_to_index(order);
        let phys_offset = crate::memory::physical_memory_offset();
        
        let virt = phys_offset + addr.as_u64();
        let block = &*(virt.as_ptr::<FreeBlock>());
        
        // Actualizar head para apuntar al siguiente
        self.free_lists[idx].head = block.next;
    }

    /// Divide un bloque grande en bloques más pequeños
    unsafe fn split_block(&mut self, from_order: usize, addr: PhysAddr, to_order: usize) {
        let mut current_order = from_order;
        let mut current_addr = addr;

        while current_order > to_order {
            current_order -= 1;
            let block_size = 1u64 << current_order;
            
            // El buddy está en current_addr + block_size
            let buddy_addr = PhysAddr::new(current_addr.as_u64() + block_size);
            
            crate::serial_println!(
                "  Split: adding buddy at {:#x}, order={}",
                buddy_addr.as_u64(), current_order
            );
            
            self.add_block(current_order, buddy_addr);
        }
    }

    /// Calcula la dirección del buddy de un bloque
    #[inline]
    fn buddy_of(&self, addr: PhysAddr, order: usize) -> PhysAddr {
        let block_size = 1u64 << order;
        PhysAddr::new(addr.as_u64() ^ block_size)
    }

    /// Verifica si un bloque está en la lista libre
    unsafe fn is_free(&self, order: usize, addr: PhysAddr) -> bool {
        let idx = self.order_to_index(order);
        let mut current = self.free_lists[idx].head;
        
        while let Some(block_addr) = current {
            if block_addr == addr {
                return true;
            }
            
            let phys_offset = crate::memory::physical_memory_offset();
            let virt = phys_offset + block_addr.as_u64();
            let block = &*(virt.as_ptr::<FreeBlock>());
            current = block.next;
        }
        
        false
    }

    /// Remueve un bloque arbitrario de la lista (no solo el head)
    unsafe fn remove_arbitrary_block(&mut self, order: usize, addr: PhysAddr) {
        let idx = self.order_to_index(order);
        let phys_offset = crate::memory::physical_memory_offset();
        
        // Caso especial: es el head
        if self.free_lists[idx].head == Some(addr) {
            self.remove_block(order, addr);
            return;
        }
        
        // Buscar en la lista
        let mut prev_addr = match self.free_lists[idx].head {
            Some(a) => a,
            None => return, // No está en la lista
        };
        
        loop {
            let prev_virt = phys_offset + prev_addr.as_u64();
            let prev_block = &mut *(prev_virt.as_mut_ptr::<FreeBlock>());
            
            match prev_block.next {
                Some(next_addr) if next_addr == addr => {
                    // Encontrado! Saltar este nodo
                    let target_virt = phys_offset + addr.as_u64();
                    let target_block = &*(target_virt.as_ptr::<FreeBlock>());
                    prev_block.next = target_block.next;
                    return;
                }
                Some(next_addr) => {
                    prev_addr = next_addr;
                }
                None => return, // No encontrado
            }
        }
    }

    /// Allocate un bloque de 2^order bytes
    /// 
    /// # Precondiciones
    /// - `order >= MIN_ORDER` (12 = 4KB)
    /// - `order <= MAX_ORDER`
    /// 
    /// # Postcondiciones
    /// - Retorna `Some(addr)` donde addr está alineado a 2^order
    /// - El bloque es exclusivo (no compartido)
    /// - El bloque está en memoria física mapeada
    /// 
    /// # Invariantes
    /// - No modifica la memoria retornada
    /// - Garantiza que no hay overlapping con otros bloques
    pub unsafe fn allocate(&mut self, order: usize) -> Option<PhysAddr> {
        // ✅ VALIDAR PRECONDICIONES
        debug_assert!(
            order >= MIN_ORDER,
            "Order {} is below MIN_ORDER {}",
            order,
            MIN_ORDER
        );
        debug_assert!(
            order <= MAX_ORDER,
            "Order {} exceeds MAX_ORDER {}",
            order,
            MAX_ORDER
        );
        
        let idx = self.order_to_index(order);

        // Caso 1: Hay un bloque del tamaño exacto
        if let Some(addr) = self.free_lists[idx].head {
            self.remove_from_head(order, addr);
            crate::serial_print_raw!("Buddy: Allocated ");
            print_hex(addr.as_u64());
            crate::serial_print_raw!(", order=");
            print_usize(order);
            crate::serial_print_raw!(" (exact fit)\n");
            return Some(addr);
        }

        // Caso 2: Split de un bloque más grande
        for larger_order in (order + 1)..=MAX_ORDER {
            let larger_idx = self.order_to_index(larger_order);
            
            if let Some(addr) = self.free_lists[larger_idx].head {
                self.remove_from_head(larger_order, addr);
                self.split_block(larger_order, addr, order);
                
                crate::serial_print_raw!("Buddy: Allocated ");
                print_hex(addr.as_u64());
                crate::serial_print_raw!(", order=");
                print_usize(order);
                crate::serial_print_raw!(" (split from ");
                print_usize(larger_order);
                crate::serial_print_raw!(")\n");
                
                return Some(addr);
            }
        }

        crate::serial_print_raw!("Buddy: OOM for order ");
        print_usize(order);
        crate::serial_print_raw!("\n");
        None
    }

    /// Libera un bloque previamente allocado
    /// 
    /// # Precondiciones
    /// - `addr` fue retornado por `allocate(order)` exacto
    /// - No ha sido liberado previamente (no double-free)
    /// - `order` es el MISMO que se usó en allocate
    /// 
    /// # Safety
    /// Violar las precondiciones causa corrupción de memoria
    pub unsafe fn deallocate(&mut self, addr: PhysAddr, order: usize) {
        // ✅ VALIDAR PRECONDICIONES
        debug_assert!(order >= MIN_ORDER);
        debug_assert!(order <= MAX_ORDER);
        
        // ✅ VALIDAR ALINEACIÓN (crítico para buddy)
        let block_size = 1u64 << order;
        debug_assert_eq!(
            addr.as_u64() % block_size,
            0,
            "Address {:#x} not aligned to order {} (block size {:#x})",
            addr.as_u64(),
            order,
            block_size
        );

        let mut current_addr = addr;
        let mut current_order = order;

        // Intentar coalescing hasta MAX_ORDER
        while current_order < MAX_ORDER {
            let buddy_addr = self.buddy_of(current_addr, current_order);
            
            if !self.is_free(current_order, buddy_addr) {
                break;
            }
            
            crate::serial_print_raw!("  Coalescing: ");
            print_hex(current_addr.as_u64());
            crate::serial_print_raw!(" + ");
            print_hex(buddy_addr.as_u64());
            crate::serial_print_raw!("\n");
            
            self.remove_arbitrary_block(current_order, buddy_addr);
            
            current_addr = PhysAddr::new(current_addr.as_u64().min(buddy_addr.as_u64()));
            current_order += 1;
        }

        crate::serial_print_raw!("Buddy: Freed ");
        print_hex(current_addr.as_u64());
        crate::serial_print_raw!(", order=");
        print_usize(current_order);
        crate::serial_print_raw!("\n");
        
        self.add_block(current_order, current_addr);
    }
    
    // ✅ RENOMBRAR para evitar confusión
    unsafe fn remove_from_head(&mut self, order: usize, addr: PhysAddr) {
        debug_assert_eq!(
            self.free_lists[self.order_to_index(order)].head,
            Some(addr),
            "Address is not at head of free list"
        );
        
        let phys_offset = crate::memory::physical_memory_offset();
        let virt = phys_offset + addr.as_u64();
        let block = &*(virt.as_ptr::<FreeBlock>());
        self.free_lists[self.order_to_index(order)].head = block.next;
    }

    /// Debug: imprime estadísticas SIN usar format!() para evitar deadlocks
    pub fn debug_print_stats(&self) {
        crate::serial_print_raw!("Buddy Allocator Stats:\n");
        crate::serial_print_raw!("  Total memory: ");
        print_u64(self.total_memory / (1024 * 1024));
        crate::serial_print_raw!("MB\n");
        
        for order in MIN_ORDER..=MAX_ORDER {
            let idx = self.order_to_index(order);
            let mut count = 0;
            
            unsafe {
                let mut current = self.free_lists[idx].head;
                while let Some(addr) = current {
                    count += 1;
                    let phys_offset = crate::memory::physical_memory_offset();
                    let virt = phys_offset + addr.as_u64();
                    let block = &*(virt.as_ptr::<FreeBlock>());
                    current = block.next;
                }
            }
            
            if count > 0 {
                let block_size = 1u64 << order;
                crate::serial_print_raw!("  Order ");
                print_usize(order);
                crate::serial_print_raw!(": ");
                print_usize(count);
                crate::serial_print_raw!(" blocks of ");
                
                if block_size >= 1024 * 1024 {
                    print_u64(block_size / (1024 * 1024));
                    crate::serial_print_raw!("MB\n");
                } else {
                    print_u64(block_size / 1024);
                    crate::serial_print_raw!("KB\n");
                }
            }
        }
    }
}

// Global instance con Mutex para thread-safety
pub static BUDDY: Mutex<BuddyAllocator> = Mutex::new(BuddyAllocator::new());

// Helper para imprimir u64 sin allocar
fn print_u64(mut n: u64) {
    if n == 0 {
        crate::serial_print_raw!("0");
        return;
    }
    
    let mut buf = [0u8; 20];
    let mut i = 0;
    
    while n > 0 {
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    
    // Imprimir al revés (más significativo primero)
    while i > 0 {
        i -= 1;
        unsafe {
            let mut port = x86_64::instructions::port::Port::<u8>::new(0x3F8);
            port.write(buf[i]);
        }
    }
}

fn print_hex(n: u64) {
    crate::serial_print_raw!("0x");
    
    let mut buf = [0u8; 16];
    let mut num = n;
    let mut i = 0;
    
    if num == 0 {
        crate::serial_print_raw!("0");
        return;
    }
    
    while num > 0 {
        let digit = (num % 16) as u8;
        buf[i] = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + (digit - 10)
        };
        num /= 16;
        i += 1;
    }
    
    while i > 0 {
        i -= 1;
        unsafe {
            let mut port = x86_64::instructions::port::Port::<u8>::new(0x3F8);
            port.write(buf[i]);
        }
    }
}

fn print_usize(n: usize) {
    print_u64(n as u64);
}