// kernel/src/memory/elf.rs
//
// ELF64 binary format parser.
//
// This module handles ONLY parsing and validation of ELF headers.
// It does not perform any memory mapping — that's elf_loader's job.
//
// Supports: ELF64, little-endian, x86_64, static executables.
// Does NOT support: dynamic linking, shared libraries, relocations.
//
// Reference: System V ABI / ELF specification
//   https://refspecs.linuxfoundation.org/elf/elf.pdf

// ============================================================================
// Constants
// ============================================================================

/// ELF magic number: 0x7F 'E' 'L' 'F'
pub const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];

// EI_CLASS values
const ELFCLASS64: u8 = 2;

// EI_DATA values
const ELFDATA2LSB: u8 = 1; // Little-endian

// e_type values
const ET_EXEC: u16 = 2; // Executable file

// e_machine values
const EM_X86_64: u16 = 62;

// Program header types
/// Loadable segment — must be mapped into memory.
pub const PT_LOAD: u32 = 1;

// Program header flags (p_flags)
/// Segment is executable.
pub const PF_X: u32 = 1 << 0;
/// Segment is writable.
pub const PF_W: u32 = 1 << 1;
/// Segment is readable.
pub const PF_R: u32 = 1 << 2;

// ============================================================================
// ELF64 Header (64 bytes)
// ============================================================================

/// The main ELF file header, always at offset 0 in the file.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct Elf64Header {
    /// Magic number and identification bytes.
    pub e_ident: [u8; 16],
    /// Object file type (ET_EXEC, ET_DYN, etc.).
    pub e_type: u16,
    /// Target architecture (EM_X86_64, etc.).
    pub e_machine: u16,
    /// ELF version (always 1).
    pub e_version: u32,
    /// Virtual address of the entry point.
    pub e_entry: u64,
    /// File offset to the program header table.
    pub e_phoff: u64,
    /// File offset to the section header table (not used by loader).
    pub e_shoff: u64,
    /// Processor-specific flags.
    pub e_flags: u32,
    /// Size of this header (should be 64 for ELF64).
    pub e_ehsize: u16,
    /// Size of one program header entry.
    pub e_phentsize: u16,
    /// Number of program header entries.
    pub e_phnum: u16,
    /// Size of one section header entry.
    pub e_shentsize: u16,
    /// Number of section header entries.
    pub e_shnum: u16,
    /// Section header string table index.
    pub e_shstrndx: u16,
}

// ============================================================================
// ELF64 Program Header (56 bytes)
// ============================================================================

/// Describes a segment to be loaded into memory.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct Elf64ProgramHeader {
    /// Segment type (PT_LOAD, PT_NOTE, etc.).
    pub p_type: u32,
    /// Segment-dependent flags (PF_R, PF_W, PF_X).
    pub p_flags: u32,
    /// Offset of the segment data in the file.
    pub p_offset: u64,
    /// Virtual address where the segment should be loaded.
    pub p_vaddr: u64,
    /// Physical address (usually same as p_vaddr, ignored for user space).
    pub p_paddr: u64,
    /// Size of the segment data in the file (may be less than p_memsz).
    pub p_filesz: u64,
    /// Size of the segment in memory (p_memsz >= p_filesz; difference is zeroed).
    pub p_memsz: u64,
    /// Alignment (must be power of 2; p_vaddr ≡ p_offset mod p_align).
    pub p_align: u64,
}

// ============================================================================
// Parsed ELF
// ============================================================================

/// A validated ELF64 binary, ready for loading.
///
/// This is a zero-copy view over the original bytes — it does NOT
/// allocate.  The lifetime `'a` ties it to the ELF data.
pub struct Elf64<'a> {
    data: &'a [u8],
    header: &'a Elf64Header,
}

impl<'a> Elf64<'a> {
    /// Parse and validate an ELF64 binary.
    ///
    /// Checks: magic, class (64-bit), endianness (little), type (executable),
    /// machine (x86_64), and that program headers are within bounds.
    ///
    /// Returns an error string on validation failure.
    pub fn parse(data: &'a [u8]) -> Result<Self, &'static str> {
        // ── Size check ────────────────────────────────────────────────
        if data.len() < core::mem::size_of::<Elf64Header>() {
            return Err("ELF: file too small for header");
        }

        // SAFETY: We verified the data is large enough.  Elf64Header is
        // repr(C, packed) so any alignment is valid.
        let header = unsafe { &*(data.as_ptr() as *const Elf64Header) };

        // ── Magic ─────────────────────────────────────────────────────
        if header.e_ident[0..4] != ELF_MAGIC {
            return Err("ELF: invalid magic number");
        }

        // ── Class: 64-bit ─────────────────────────────────────────────
        if header.e_ident[4] != ELFCLASS64 {
            return Err("ELF: not a 64-bit binary");
        }

        // ── Endianness: little-endian ─────────────────────────────────
        if header.e_ident[5] != ELFDATA2LSB {
            return Err("ELF: not little-endian");
        }

        // ── Type: executable ──────────────────────────────────────────
        if header.e_type != ET_EXEC {
            return Err("ELF: not an executable (ET_EXEC)");
        }

        // ── Machine: x86_64 ──────────────────────────────────────────
        if header.e_machine != EM_X86_64 {
            return Err("ELF: not x86_64");
        }

        // ── Program header table bounds ───────────────────────────────
        let ph_start = header.e_phoff as usize;
        let ph_size = (header.e_phentsize as usize) * (header.e_phnum as usize);
        let ph_end = ph_start.checked_add(ph_size)
            .ok_or("ELF: program header table overflows")?;

        if ph_end > data.len() {
            return Err("ELF: program header table extends past end of file");
        }

        if (header.e_phentsize as usize) < core::mem::size_of::<Elf64ProgramHeader>() {
            return Err("ELF: program header entry too small");
        }

        Ok(Self { data, header })
    }

    // ====================================================================
    // Accessors
    // ====================================================================

    /// Virtual address of the entry point (_start).
    #[inline]
    pub fn entry_point(&self) -> u64 {
        self.header.e_entry
    }

    /// Number of program headers.
    #[inline]
    pub fn ph_count(&self) -> usize {
        self.header.e_phnum as usize
    }

    /// File offset of the program header table (e_phoff).
    #[inline]
    pub fn phdr_file_offset(&self) -> u64 {
        self.header.e_phoff
    }

    /// Get the i-th program header.
    ///
    /// Panics if `index >= ph_count()`.
    pub fn program_header(&self, index: usize) -> &'a Elf64ProgramHeader {
        assert!(index < self.ph_count(), "ELF: program header index out of bounds");
        let offset = self.header.e_phoff as usize
            + index * (self.header.e_phentsize as usize);
        // SAFETY: bounds checked in parse() and by the assert above.
        unsafe { &*(self.data.as_ptr().add(offset) as *const Elf64ProgramHeader) }
    }

    /// Iterator over all program headers.
    pub fn program_headers(&self) -> ProgramHeaderIter<'a> {
        ProgramHeaderIter {
            data: self.data,
            offset: self.header.e_phoff as usize,
            entry_size: self.header.e_phentsize as usize,
            remaining: self.header.e_phnum as usize,
        }
    }

    /// Iterator over only PT_LOAD segments (the ones that need mapping).
    pub fn load_segments(&self) -> impl Iterator<Item = &'a Elf64ProgramHeader> {
        self.program_headers().filter(|ph| ph.p_type == PT_LOAD)
    }

    /// Get the raw file data for a program header's segment.
    ///
    /// Returns a slice of `p_filesz` bytes starting at `p_offset`.
    /// Returns `None` if the segment extends past end of file.
    pub fn segment_data(&self, ph: &Elf64ProgramHeader) -> Option<&'a [u8]> {
        let start = ph.p_offset as usize;
        let end = start.checked_add(ph.p_filesz as usize)?;
        if end > self.data.len() {
            return None;
        }
        Some(&self.data[start..end])
    }

    /// Total raw ELF data.
    #[inline]
    pub fn raw_data(&self) -> &'a [u8] {
        self.data
    }
}

// ============================================================================
// Program header iterator
// ============================================================================

pub struct ProgramHeaderIter<'a> {
    data: &'a [u8],
    offset: usize,
    entry_size: usize,
    remaining: usize,
}

impl<'a> Iterator for ProgramHeaderIter<'a> {
    type Item = &'a Elf64ProgramHeader;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        // SAFETY: bounds were validated in Elf64::parse().
        let ph = unsafe {
            &*(self.data.as_ptr().add(self.offset) as *const Elf64ProgramHeader)
        };
        self.offset += self.entry_size;
        self.remaining -= 1;
        Some(ph)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<'a> ExactSizeIterator for ProgramHeaderIter<'a> {}