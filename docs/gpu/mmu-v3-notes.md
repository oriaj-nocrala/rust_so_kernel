# GA10x MMU (v3 = "GP100 v2 format") — design notes for phase 6

Read from nouveau Linux v7.2.2 (`nvkm/subdev/mmu/{vmmgp100.c,vmmgv100.c,vmmtu102.c,vmmgf100.c}`, `nvkm/subdev/gsp/rm/r535/vmm.c`) and decoded from `trace-gsp`. **Nothing of this is implemented yet**: it is the notes for `nvgpu/src/mmu.rs` (pure page-table builder + walker, host-tested), the largest new module of phase 6. Companion: `docs/gpu/phase6-oracle.md`.

## Two ways to get a VA space from RM (both are in the trace)

1. **External (we own the page tables)**, `r535_mmu_vaspace_new(.., external = true)` = trace #11 + #12:
   - `RM_ALLOC FERMI_VASPACE_A` (`0x90f1`), obj `0x90f10000`, parent = device, params 48 B = `NV_VASPACE_ALLOCATION_PARAMETERS`: `index = 0` (`GPU_NEW`), `flags = 0x8` (`IS_EXTERNALLY_OWNED`), rest 0 (bytes in `nvgpu/fixtures/rm-vaspace-req.bin`).
   - `RM_CONTROL NV0080_CTRL_CMD_DMA_SET_PAGE_DIRECTORY` (`0x00801813`) **on the device**, 32 B: `physAddress` (u64; the trace has `0x1_f07d_1000`, a VRAM address), `numEntries = 4`, `flags = 0` (aperture VIDMEM), `hVASpace = 0x90f10000`, `pasid = 0` (`rm-ctrl801813-req.bin`).
   - `numEntries = 1 << page[0].desc->bits` = the entries of the **root** directory: 4 (2 bits) — the top of the 49-bit tree.
   - This is the mode to use: we build the tables (in VRAM through PRAMIN/BAR1, or in system memory), RM only needs the root's address. Teardown = `NV0080_CTRL_CMD_DMA_UNSET_PAGE_DIRECTORY` then `FREE`.
2. **RM-owned** (trace #19 + #20): same ALLOC with `flags = 0`, then `NV90F1_CTRL_CMD_VASPACE_COPY_SERVER_RESERVED_PDES` (`0x90f10106`, 184 B: `pageSize 512 MiB`, `virtAddrLo/Hi` = `0x1_0000_0000 .. +512 MiB-1` (`SPLIT_VAS_SERVER_RM_MANAGED_VA_START/SIZE`, `r535/nvrm/vmm.h:28`), `numLevelsToCopy`, per level `physAddress/size/aperture/pageShift`): nouveau builds the tables, RM manages a reserved 512 MiB window in it. Only needed if a channel must use RM's server-reserved VA; skip for a first copy.

## Table format (GP100 "v2", also GA10x: `tu102_vmm.page[]`, `vmmtu102.c:52-60`)

VA = 49 bits. Levels from the root (`gp100_vmm_desc_12`/`_16`, `vmmgp100.c:403-418`), `{type, bits, entry size, table size}`:

| Level | index bits | entry | table size | covers per entry |
|---|---|---|---|---|
| PD3 (root) | 2 (VA 48:47) | 8 B | 0x1000 | 128 TiB |
| PD2 | 9 (46:38) | 8 B | 0x1000 | 256 GiB |
| PD1 | 9 (37:29) | 8 B | 0x1000 | 512 MiB |
| PD0 | 8 (28:21) | **16 B** (small-page PDE + big-page PDE) | 0x1000 | 2 MiB |
| PT small (4 KiB pages) | 9 (20:12) | 8 B | 0x1000 | 4 KiB |
| PT big (64 KiB pages, "LPT") | 5 (20:16) | 8 B | 0x100 | 64 KiB |

Page sizes offered (`page[]`, shift): 47, 38, 29 (huge leaf entries at PD3/PD2/PD1, "Sxxx"), **21 (2 MiB, leaf in PD0)**, 16 (64 KiB), **12 (4 KiB)**. For a first version: 4 KiB pages (`desc_12`) and optionally 2 MiB leaves in PD0.

### Entries (little-endian u64)

- **PTE** (`gp100_vmm_valid`, `vmmgp100.c:423-497`; `gp100_vmm_pgt_pte`): `data = (phys_addr >> 4) | type`, with `type` = `VALID` bit 0 | `aperture << 1` (bits 2:1: **VRAM 0, system coherent 2, system non-coherent 3**, `gf100_vmm_aper`) | `VOL` bit 3 (set for host memory) | `PRIV` bit 5 | `RO` bit 6 | `ATOMIC_DISABLE` bit 7 | `kind << 56` (kind 0 = pitch-linear). The physical address is `>> 4`, i.e. bits 63:8 hold `addr >> 12`. Next page: `+ (1 << shift) >> 4`. Compression (comptag, `<< 36`) is not needed: use kind 0.
- **PDE** (`gp100_vmm_pde`, `vmmgp100.c:237-251`): `data = (child_table_phys >> 4) | aperture_bits`, aperture in bits 2:1 with **VRAM = 1, host = 2 (+ `VOL` bit 3), non-coherent = 3** (different numbering from the PTE!). No valid bit: a zero PDE is "not present". PD0 writes 16 bytes: `data[0]` = small-page table PDE, `data[1]` = big-page table PDE (0 if none).
- **Sparse** (not needed): PTE `VOL` without `VALID`; PD0 `VOL_BIG` (bit 3).
- A leaf at a higher level (2 MiB in PD0, 512 MiB in PD1, ...) is a PTE written in that level's slot (`gp100_vmm_pd0_mem`, 16-byte entry of which the first 8 bytes carry it).

## Making the GPU see it

- The channel's instance block gets the page-directory base (`gv100_vmm_join`, `vmmgv100.c:31-60`, `gp100_vmm_join`): with GSP-RM this is done **by RM** when the channel is allocated with `hVASpace` = our vaspace (`NV_CHANNEL_ALLOC_PARAMS.hVASpace`); nouveau does not write the instance block itself in RM mode. So no `join` port is needed.
- TLB invalidation after writing tables: `tu102_vmm_flush` (`vmmtu102.c:26-43`): write `pd_addr >> 8` to `0xb830a0`, `0` to `0xb830a4`, `0x80000000 | type` to `0xb830b0` (`type = 1` PAGE_ALL; `+ 6` HUB_ONLY|ALL_PDB when BAR is involved), poll bit 31 clear (2 s). For a fresh VA space that no channel used yet, the flush before the first channel start is the safe choice; with `bar2_pdb` from the static info (`gsp->bar.rm_bar2_pdb`) nouveau flushes that PDB instead (BAR2 is RM's).
- VRAM tables: write them through the PRAMIN window (`nvgpu::evo::Pramin`, already used for the display instance memory: BAR0 `0x700000`, `0x1700 << 16` selects the VRAM window) or the BAR1 WC mapping (only the first 16 MiB are mapped today, see `kernel/src/gpu/mod.rs` `BAR1_WINDOW`). Keep tables away from the carve-out used by the display and GSP: VRAM map so far (`docs/reference/gpu.md`, `gpu-gsp` skill): GOP 0.., scanout 16/32 MiB, HDMI 48 MiB, display instance memory `0x1ffc90000`, **WPR2 `0x1f4100000 .. 0x1fff00000`**, non-WPR heap `0x1f4000000`. A free range such as `[64 MiB, 128 MiB)` is unused.

## Suggested `nvgpu/src/mmu.rs`

- `struct PageTables` = `BTreeMap<phys_addr, [u8; 4096]>` (or a Vec of tables) with a bump allocator over a caller-given physical range; `map(va, pa, aperture, flags)`, `map_range`, 4 KiB and 2 MiB.
- `walk(root, va) -> Option<(pa, flags)>` written **independently** from `map` (straight from the format above) and used by the tests; also a `translate` sabotage-friendly API.
- Tests: hand-computed PTE/PDE words from the bit layout above (independent of the code); a page-table image compared byte-for-byte with what nouveau would write is not available (no MMU trace: nouveau writes tables through BAR2/PRAMIN, not in mmiotrace), so the oracle is (a) the format, (b) the physical address RM accepted in `SET_PAGE_DIRECTORY`, (c) on metal a copy through the channel that reads/writes a known pattern. Sabotage every shift, aperture and bit.
- Kernel adapter: allocate the table pages in VRAM (PRAMIN writes, bounded), send `ALLOC 0x90f1` + `SET_PAGE_DIRECTORY`, flush.
