// kernel/src/allocator/mod.rs
//
// CORRECTED: Removed FRAME_ALLOCATOR and PAGE_TABLE globals.
//
// Previous bug:
//   BootInfoFrameAllocator was initialized over the SAME physical memory
//   regions as the Buddy allocator.  Both could hand out the same frame.
//   In practice this didn't explode because nothing read FRAME_ALLOCATOR
//   after init — but the globals were public and accessible, making it a
//   latent corruption vector.
//
// Current design:
//   - Buddy allocator is the SOLE owner of physical memory after init.
//   - BootInfoFrameAllocator exists as a type (for potential early-boot use)
//     but is NOT stored globally.
//   - Page table operations go through OwnedPageTable (page_table_manager.rs).

pub mod buddy_allocator;
pub mod slab;