# Memory

Code: `kernel/src/memory/`, `kernel/src/allocator/`, crate `mm/` (host tests: `cd mm && cargo test`).

## Allocators

- **Buddy** (`mm::buddy`, physical frames, orders 12–28 = 4 KiB–256 MiB). Global `BUDDY` in `kernel/src/allocator/mod.rs` is the **only** frame allocator after `init_core`.
- **Slab** (`mm::slab`, kernel heap) is the `#[global_allocator]` (`SlabGlobalAlloc`). Gets frames only through `mm::FrameSource` → `phys_alloc`/`phys_free`.
- **`mm` has no `alloc` dependency, on purpose**: it *is* the allocator, so any allocation inside it would recurse. Fixed arrays and intrusive lists only.
- The buddy free lists are **doubly linked** (`FreeBlock { next, prev }` in the free block itself), so coalescing removes the buddy from the middle of a list in O(1). With `next` only it scanned from the head and gave up after 4096 links (`PhantomEvent::LoopLimit`, seen once on the Ryzen with a long order-12 list); that variant is gone. A phantom bitmap bit is now caught by the neighbours not pointing back (`NotFound`).
- `kernel/src/allocator/mod.rs` is the adapter: owns the globals and turns `mm`'s events (`PhantomEvent`, `AllocEvent`/`DeallocEvent`) into logs. Failures always print; routine events are `ktrace!(MM)`. A double free panics.
- `BUDDY`/`SLAB_ALLOCATOR` are `diag::IrqMutex` (see Key Design Invariants in CLAUDE.md).
- `mm` uses `x86_64::PhysAddr`/`VirtAddr`, pinned `=0.15.4` (0.15.5 does not build on this nightly).
- **Known limit:** the buddy's free bitmap only covers 0–512 MiB. Above that the allocator is still correct (the free list is the source of truth), but it never coalesces and can't detect a double free. On the Ryzen all RAM is above 512 MiB. Watch for large (1 MiB) allocations failing over time.

## Address spaces

- `OwnedPageTable` (`page_table_manager.rs`) wraps `OffsetPageTable`. `new_user()` copies the kernel's (non-user) PML4 entries into a fresh PML4.
- `AddressSpace` (`address_space.rs`) = page table + `VmaList`. Each process holds one through an `Arc` (threads share it).
- **VMAs** (`vma.rs`): at most 65530 per process (Linux's `vm.max_map_count`), in an unsorted `Vec` (lookups are linear: keep it small). `mmap` places anonymous mappings contiguously and `VmaList::add_merged` folds a new `Anonymous` VMA into an adjacent one with equal flags (there was a guard page between allocations and a cap of 256, which a tokio run with 4000 tasks hit through musl's allocator). `/proc/<pid>/maps` lists them. Kinds:
  - `Code`: loaded up front, not demand-paged.
  - `Anonymous`: zero-filled on demand.
  - `GrowableStack`: starts at 64 KiB and grows down on a fault in the guard gap, up to 8 MiB (`VmaList::grow_stack`, called from the fault path).
  - `Huge2M`: any anonymous `mmap` of 2 MiB or more. Whole 2 MiB pages; touching one byte makes all 512 resident. There is no huge zero page.
  - `Shared`: see Shared memory below.
- **NX:** user pages are `NO_EXECUTE` unless their ELF segment is `PF_X` or their mapping has `PROT_EXEC` (`prot_to_flags`, `elf_flags_to_page_flags`); the stack is NX; the signal trampoline page (`TRAMPOLINE_VA`) stays executable. `EFER.NXE` is verified on every CPU (`cpu/init.rs`). Every PTE takes its flags from the VMA (fork, COW, demand paging, the zero-frame mapping minus `WRITABLE`), so NX survives them all. An instruction fetch from an NX page kills with `EXECUTED NON-EXECUTABLE MEMORY at <addr>` (`SIGSEGV`/`SEGV_ACCERR`, rip = si_addr). Test: `nx_test`.
- **Protection:** a VMA's `flags` are its PTE flags. `PROT_NONE` = `PRESENT` without `USER_ACCESSIBLE`; `map_demand_page` refuses such a VMA (not even the zero frame), so touching one kills the process. `mmap` of `PROT_NONE` is never `Huge2M` (a reservation that `mprotect` will cut).
- **`mprotect`/`munmap` cut VMAs** (`VmaList::split_at`, `remove_range`, `merge_adjacent`; only `Anonymous` neighbours are rejoined). `mprotect` leaves a read-only PTE read-only even when the VMA becomes writable: the first write goes through the COW path, which is what makes a zero-frame or shared page private. Lowering clears `WRITABLE`/`USER` in every present PTE at once. `fork` derives the child's PTEs from the VMA flags, so a test that forks first cannot see whether `mprotect` updated the parent's PTEs (`mprotect_test` maps and lowers inside the child for that).
- **The address-space lock** (`AddressSpace::vmas`, an `IrqMutex`) covers every VMA lookup and every PTE change: faults, COW, fork's write-protect, `mmap`/`munmap`. A fault that finds its page already mapped (another thread won) counts as success.
  - Lock order: scheduler → address space → `BUDDY`/`SLAB_ALLOCATOR`.
  - Never touch user memory by virtual address while holding it: the fault would take the lock again.
- **Page-table levels are always `PRESENT | WRITABLE` (+ `USER` for a user page); the leaf alone sets the permissions.** Map with `map_to_with_table_flags(.., OwnedPageTable::table_flags_for(flags), ..)`, never x86_64's plain `map_to`, which copies the leaf's flags into the levels it creates: a read fault that mapped the zero frame (read-only) first in a fresh 2 MiB made its page table read-only, and every later write there faulted with the PTE already writable, which `make_writable_locked` reports as done, so the process looped on the fault forever (test: `zeropage_test`, proven by reverting the fix).
- **Writing another process's memory:** use `AddressSpace::copy_to_user`/`copy_from_user`/`prepare_user_write`. **Never** `translate_page` + a physmap write: the frame behind the page may be the shared zero frame or a COW frame still shared with a fork sibling (test: `pipe_cow_test`).

## Page faults

- `init/devices.rs` reads CR2 and calls `handle_not_present_fault`/`handle_cow_fault` on the *running process's* address space, under its lock. Pages are mapped into that table, not into whatever CR3 holds.
- A fault in kernel mode panics. A fault in user mode outside every VMA kills the process. An instruction fetch from a present NX page (error code `P|I`) is named as such in the log and the kill notice.

## COW refcounts (`memory/cow.rs`)

- One `AtomicU8` per physical frame. The table is sized at boot from the highest usable address (`init_refcount_table` in `init_core`). `cow_tracked_frames` in `/proc/kdebug` shows its coverage.
- Convention: a count of 1 means "I am the last owner", 0 means "free it". Decisions on a count are made under the address-space lock. `fork`, the only way a count rises, holds that same lock.
- An index outside the table **fails safe**: `get_ref` returns 2, so COW copies; `dec_ref` returns 1, so nothing is freed. (A fixed 512 MiB table once broke `fork` on every machine with more RAM.)
- `fork` **copies** `Huge2M` pages (`fork_copy_huge`); they are never shared.

## Shared memory (`memory/shm.rs`, `ipc/memfd.rs`)

- A `ShmObject` is a size plus one lazily allocated frame per page. You get one from `memfd_create` or from `mmap(MAP_SHARED|MAP_ANONYMOUS)`.
- A `Shared` VMA maps the object's own frames: never the zero frame, never COW, and `fork` does not write-protect them.
- Frame lifetime rides on the COW refcounts: the object holds one reference per frame and each PTE holds another. Because counts are `u8`, one object allows at most `MAX_MAPPINGS` (200) mappings; past that `mmap` returns `ENOMEM` and `fork` fails.
- `ftruncate` that shrinks a mapped object returns `EBUSY`.
- Lock order: address space → `ShmObject::inner` → `BUDDY`.
- `sys_mmap` gets the object through `FileHandle::shm_object()` (an `Arc<dyn Any>`, downcast).
- Test: `shm_test`.

## ELF loader (`memory/elf_loader.rs`)

- Segments without `PF_X` are NX. A page shared by two segments with different permissions keeps the first one's (and a serial warning says so); none of our binaries has one (checked: 84 ELFs).
- Static ELF64 only; reads only the ELF header and the PT_LOAD program headers (and looks for PT_INTERP, which is refused), so stripped binaries load the same.
- `ET_DYN` (static-pie, Rust's default for musl) loads at the fixed base `PIE_BASE` (4 GiB): segments, entry and `AT_PHDR` get the bias, and **the kernel applies no relocations** (musl's rcrt1 does). No ASLR. Test: `pie_test` (freestanding, `DISK_PIE_PROGRAMS` in `kernel/build.rs`); a real `x86_64-unknown-linux-musl` std binary runs too (`rustc` inside the repo dir picks the pinned nightly).
- auxv: `AT_PHDR/PHENT/PHNUM/ENTRY/PAGESZ/RANDOM`. `AT_RANDOM` points at 16 fresh bytes in the stack page.
- `build_initial_stack` writes the SysV argc/argv/envp/auxv frame into the top stack page. If it does not fit in one page, exec fails with `E2BIG`.

## RSS (`hal::paging::count_resident`, `AddressSpace::mem_stats`)

- `/proc/<pid>/stat`'s `rss` and `/proc/<pid>/statm` are computed by walking the page tables under the address-space lock, not by keeping a counter (a counter would need every PTE-changing path to update it).
- A 2 MiB leaf counts as 512 pages; the zero frame is not counted. `shared` = the resident part of `Shared` VMAs; `text`/`data` are virtual sizes.
- procfs clones the `Arc<AddressSpace>` under the scheduler lock and walks it after releasing that lock.
- Test: `rss_test`.
