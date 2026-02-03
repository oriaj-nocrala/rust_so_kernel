use x86_64::PhysAddr;

const MIN_ORDER: usize = 12; // 4KB
const MAX_ORDER: usize = 19; //512 KB para probar, deberia ser log2(max_ram);

pub struct Buddy {
    free_lists: [FreeList; MAX_ORDER + 1], 
}

#[derive(Clone, Copy)]
struct FreeList{
    head: Option<PhysAddr>,
}

struct FreeBlock{
    next: Option<PhysAddr>,
}

impl Buddy {
    pub const fn new() -> Self {
        Self {
            free_lists: [FreeList { head: Some(PhysAddr::zero()) }; MAX_ORDER + 1],
        }
    }

    pub unsafe fn add_region(&mut self, start: u64, end: u64) {
        let mut current_addr = start;
        let mut remaining_size = end - start;

        while remaining_size >= (1 << MIN_ORDER) {
            // 1. Calculamos el "Order" mas grande que permite la direccion actual
            // (Alineacion) y el tama;o restante.
            let align_order = current_addr.trailing_zeros() as usize;
            let size_order = 63 - remaining_size.leading_zeros() as usize;

            // El orden a usar es el mas grande posible que no exceda ni la
            // alineacion ni el tama;o disponible.
            let order = align_order.min(size_order).min(MAX_ORDER);

            if order >= MIN_ORDER {
                self.add_block(order, PhysAddr::new(current_addr));
                let block_size = 1 << order;
                current_addr += block_size;
                remaining_size -= block_size;
            } else {
                // Si es mas peque;o que 4KB, no podemos usarlo en este Buddy
                // (O podrias redondear la direccion al siguiente 4KB)
                break;
            }
        }
    }
    unsafe fn add_block(&mut self, order: usize, addr: PhysAddr) {
        // Convertir fisica a virtual usando el offset
        let phys_offset = crate::memory::physical_memory_offset();
        let virt_addr = phys_offset + addr.as_u64();

        // Creamos un bloque que apunta al actual 'head' de la lista
        let new_block = FreeBlock {
            next: self.free_lists[order].head,
        };

        // Escritura Zero-Copy: Escribimos el struct FreeBlock en la direccion VIRTUAL->FISICA
        // let ptr = addr.as_u64() as *mut FreeBlock;
        let ptr = virt_addr.as_mut_ptr::<FreeBlock>();
        ptr.write(new_block);

        // Actualizamos el head de la lista para que apunte a este nuevo bloque
        self.free_lists[order].head = Some(addr);
    }

    pub unsafe fn allocate(&mut self, order: usize) -> Option<PhysAddr> {
        // Buscar en orden exacto
        if let Some(addr) = self.free_lists[order].head {
            self.remove_block(order, addr);
            return Some(addr);
        }
        
        // Split de orden mayor
        for larger_order in (order + 1)..=MAX_ORDER {
            if let Some(addr) = self.free_lists[larger_order].head {
                self.remove_block(larger_order, addr);
                self.split_block(larger_order, addr, order);
                return Some(addr);
            }
        }
        
        None
    }
    
    unsafe fn remove_block(&mut self, order: usize, addr: PhysAddr) {
        let phys_offset = crate::memory::physical_memory_offset();
        let virt = phys_offset + addr.as_u64();
        let block = &*(virt.as_ptr::<FreeBlock>());
        self.free_lists[order].head = block.next;
    }
    
    unsafe fn split_block(&mut self, from: usize, addr: PhysAddr, to: usize) {
        let mut current_order = from;
        let mut current_addr = addr;
        
        while current_order > to {
            current_order -= 1;
            let buddy_addr = current_addr + (1 << current_order);
            self.add_block(current_order, buddy_addr);
        }
    }
}