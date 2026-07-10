//! Read-only, fail-closed view of a 64-bit little-endian ELF file.
//!
//! Decodes the file header, program headers, section headers, section-name
//! strings, and symbol tables — enough to name and bound the code regions a
//! disassembler walks (see the ELF-64 object file format specification,
//! TIS ELF v1.2 and the ELF64 supplement). The view is *lazy*: parsing
//! validates the header and table bounds once, and each header, section,
//! name, or symbol is decoded on access with its own bounds check, so a
//! file with a huge symbol table costs only the entries actually read.
//!
//! Every count is additionally capped by a fixed validation bound
//! ([`MAX_PROGRAM_HEADERS`], [`MAX_SECTIONS`], [`MAX_SYMBOLS`],
//! [`MAX_NAME`]); the caps are security limits on untrusted input, not
//! growable capacities. A malformed input is a typed [`ElfError`] naming
//! what failed — never a panic and never a partial trust of later bytes.

/// `e_ident[EI_CLASS]` value for a 64-bit ELF file.
pub const ELF_CLASS_64: u8 = 2;

/// `e_ident[EI_DATA]` value for a little-endian ELF file.
pub const ELF_DATA_LSB: u8 = 1;

/// Size of an ELF64 file header (`e_ehsize`).
pub const ELF_HEADER_LEN: usize = 64;

/// Size of one ELF64 program header (`e_phentsize`).
pub const PROGRAM_HEADER_LEN: usize = 56;

/// Size of one ELF64 section header (`e_shentsize`).
pub const SECTION_HEADER_LEN: usize = 64;

/// Size of one ELF64 symbol-table entry (`sh_entsize` of a symbol table).
pub const SYMBOL_LEN: usize = 24;

/// Fixed validation cap on `e_phnum`.
pub const MAX_PROGRAM_HEADERS: usize = 512;

/// Fixed validation cap on `e_shnum`.
pub const MAX_SECTIONS: usize = 4096;

/// Fixed validation cap on the entry count of one symbol table.
pub const MAX_SYMBOLS: usize = 1 << 20;

/// Fixed validation cap on the byte length of one string-table string.
pub const MAX_NAME: usize = 4096;

/// `sh_type` of a symbol table (`SHT_SYMTAB`).
pub const SHT_SYMTAB: u32 = 2;

/// `sh_type` of a string table (`SHT_STRTAB`).
pub const SHT_STRTAB: u32 = 3;

/// `sh_type` of a section that occupies no file bytes (`SHT_NOBITS`).
pub const SHT_NOBITS: u32 = 8;

/// `sh_type` of a dynamic-linking symbol table (`SHT_DYNSYM`).
pub const SHT_DYNSYM: u32 = 11;

/// Why an ELF input was refused.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ElfError {
    /// The input is shorter than the structure it must contain.
    TooSmall,
    /// The `\x7fELF` magic is absent.
    BadMagic,
    /// Not a 64-bit little-endian version-1 ELF file.
    UnsupportedLayout,
    /// `e_machine` is not one this crate decodes.
    UnsupportedMachine(u16),
    /// `e_ehsize`, `e_phentsize`, or `e_shentsize` disagrees with ELF64.
    BadEntrySize,
    /// A header table's count exceeds its fixed validation cap.
    TableTooLarge,
    /// A declared table or section extent falls outside the file.
    OutOfBounds,
    /// ELF extended numbering (`PN_XNUM` / `SHN_LORESERVE` escapes) —
    /// refused rather than half-decoded.
    UnsupportedExtension,
    /// An index names no entry in its table.
    BadIndex,
    /// A section is not the kind the accessor requires (e.g. a symbol
    /// table was requested from a non-symbol section).
    WrongSectionType,
    /// A string is unterminated within [`MAX_NAME`], out of bounds, or
    /// not valid UTF-8.
    BadString,
}

impl core::fmt::Display for ElfError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooSmall => f.write_str("input shorter than required structure"),
            Self::BadMagic => f.write_str("missing ELF magic"),
            Self::UnsupportedLayout => f.write_str("not a 64-bit little-endian version-1 ELF"),
            Self::UnsupportedMachine(m) => write!(f, "unsupported machine {m}"),
            Self::BadEntrySize => f.write_str("header entry size disagrees with ELF64"),
            Self::TableTooLarge => f.write_str("table count exceeds validation cap"),
            Self::OutOfBounds => f.write_str("declared extent falls outside the file"),
            Self::UnsupportedExtension => f.write_str("ELF extended numbering is not supported"),
            Self::BadIndex => f.write_str("index names no table entry"),
            Self::WrongSectionType => f.write_str("section is not of the required type"),
            Self::BadString => f.write_str("malformed string-table string"),
        }
    }
}

/// The instruction-set architecture an ELF file targets (`e_machine`),
/// restricted to the Tier-1 machines this crate decodes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Machine {
    /// `EM_X86_64` (62).
    X86_64,
    /// `EM_AARCH64` (183).
    Aarch64,
    /// `EM_RISCV` (243).
    Riscv64,
}

impl Machine {
    fn from_e_machine(raw: u16) -> Result<Self, ElfError> {
        match raw {
            62 => Ok(Self::X86_64),
            183 => Ok(Self::Aarch64),
            243 => Ok(Self::Riscv64),
            other => Err(ElfError::UnsupportedMachine(other)),
        }
    }

    /// Human-readable architecture name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
            Self::Riscv64 => "riscv64",
        }
    }
}

/// The decoded ELF64 file header.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ElfHeader {
    /// Object-file type (`e_type`: 2 executable, 3 shared object, …).
    pub e_type: u16,
    /// Target instruction-set architecture.
    pub machine: Machine,
    /// Entry-point virtual address (`e_entry`).
    pub entry: u64,
    /// Processor-specific flags (`e_flags`).
    pub flags: u32,
    /// Program-header table offset (`e_phoff`).
    pub phoff: u64,
    /// Section-header table offset (`e_shoff`).
    pub shoff: u64,
    /// Number of program headers (`e_phnum`).
    pub phnum: u16,
    /// Number of section headers (`e_shnum`).
    pub shnum: u16,
    /// Index of the section-name string table (`e_shstrndx`).
    pub shstrndx: u16,
}

/// One decoded ELF64 program header.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ProgramHeader {
    /// Segment type (`p_type`: 1 `PT_LOAD`, 2 `PT_DYNAMIC`, …).
    pub p_type: u32,
    /// Permission flags (`p_flags`: bit 0 X, bit 1 W, bit 2 R).
    pub flags: u32,
    /// File offset of the segment's bytes (`p_offset`).
    pub offset: u64,
    /// Virtual load address (`p_vaddr`).
    pub vaddr: u64,
    /// Physical address, where meaningful (`p_paddr`).
    pub paddr: u64,
    /// Number of file bytes (`p_filesz`).
    pub file_size: u64,
    /// Number of memory bytes (`p_memsz`).
    pub mem_size: u64,
    /// Required alignment (`p_align`).
    pub align: u64,
}

/// One decoded ELF64 section header.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Section {
    /// Offset of the section's name in the section-name string table
    /// (`sh_name`).
    pub name_offset: u32,
    /// Section type (`sh_type`).
    pub sh_type: u32,
    /// Attribute flags (`sh_flags`: bit 0 W, bit 1 A, bit 2 X).
    pub flags: u64,
    /// Virtual address when mapped (`sh_addr`).
    pub addr: u64,
    /// File offset of the section's bytes (`sh_offset`).
    pub offset: u64,
    /// Byte size (`sh_size`).
    pub size: u64,
    /// Linked section index (`sh_link`).
    pub link: u32,
    /// Extra type-specific information (`sh_info`).
    pub info: u32,
    /// Required alignment (`sh_addralign`).
    pub align: u64,
    /// Per-entry size for table sections (`sh_entsize`).
    pub entsize: u64,
}

/// One decoded ELF64 symbol-table entry.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Symbol {
    /// Offset of the symbol's name in the linked string table (`st_name`).
    pub name_offset: u32,
    /// Type and binding (`st_info`).
    pub info: u8,
    /// Visibility (`st_other`).
    pub other: u8,
    /// Defining section index (`st_shndx`).
    pub shndx: u16,
    /// Symbol value — usually a virtual address (`st_value`).
    pub value: u64,
    /// Associated size in bytes (`st_size`).
    pub size: u64,
}

/// Little-endian `u16` at `offset`, bounds-checked.
fn u16_at(bytes: &[u8], offset: usize) -> Result<u16, ElfError> {
    let end = offset.checked_add(2).ok_or(ElfError::OutOfBounds)?;
    let raw = bytes.get(offset..end).ok_or(ElfError::TooSmall)?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

/// Little-endian `u32` at `offset`, bounds-checked.
fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, ElfError> {
    let end = offset.checked_add(4).ok_or(ElfError::OutOfBounds)?;
    let raw = bytes.get(offset..end).ok_or(ElfError::TooSmall)?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

/// Little-endian `u64` at `offset`, bounds-checked.
fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, ElfError> {
    let end = offset.checked_add(8).ok_or(ElfError::OutOfBounds)?;
    let raw = bytes.get(offset..end).ok_or(ElfError::TooSmall)?;
    Ok(u64::from_le_bytes([
        raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
    ]))
}

/// Whether the extent `offset..offset + len` lies inside a file of
/// `file_len` bytes, computed in `u64` so no width truncation can pass a
/// hostile extent.
fn extent_in_file(offset: u64, len: u64, file_len: usize) -> bool {
    match offset.checked_add(len) {
        Some(end) => end <= file_len as u64,
        None => false,
    }
}

/// The file-offset byte slice `offset..offset + len`, bounds-checked.
fn file_slice(bytes: &[u8], offset: u64, len: u64) -> Result<&[u8], ElfError> {
    if !extent_in_file(offset, len, bytes.len()) {
        return Err(ElfError::OutOfBounds);
    }
    let start = usize::try_from(offset).map_err(|_| ElfError::OutOfBounds)?;
    let count = usize::try_from(len).map_err(|_| ElfError::OutOfBounds)?;
    bytes.get(start..start + count).ok_or(ElfError::OutOfBounds)
}

/// The NUL-terminated UTF-8 string at `offset` in `strtab`, capped at
/// [`MAX_NAME`] bytes.
fn read_string(strtab: &[u8], offset: u32) -> Result<&str, ElfError> {
    let start = usize::try_from(offset).map_err(|_| ElfError::BadString)?;
    if start > strtab.len() {
        return Err(ElfError::BadString);
    }
    let window_end = start.saturating_add(MAX_NAME).min(strtab.len());
    let window = &strtab[start..window_end];
    let nul = window
        .iter()
        .position(|&b| b == 0)
        .ok_or(ElfError::BadString)?;
    core::str::from_utf8(&window[..nul]).map_err(|_| ElfError::BadString)
}

/// A validated, lazy view of a 64-bit little-endian ELF file.
///
/// [`ElfView::parse`] validates the file header and both header tables'
/// bounds once; every per-entry accessor then decodes on demand with its
/// own bounds check, so cost follows what the caller reads, not the file.
#[derive(Copy, Clone, Debug)]
pub struct ElfView<'a> {
    bytes: &'a [u8],
    header: ElfHeader,
}

impl<'a> ElfView<'a> {
    /// Decode and validate `bytes` as an ELF64 little-endian file.
    ///
    /// # Errors
    ///
    /// A typed [`ElfError`] naming the first violated invariant; the
    /// input is rejected whole.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ElfError> {
        if bytes.len() < ELF_HEADER_LEN {
            return Err(ElfError::TooSmall);
        }
        if bytes[0..4] != *b"\x7fELF" {
            return Err(ElfError::BadMagic);
        }
        // Identification: 64-bit, little-endian, ident version 1.
        if bytes[4] != ELF_CLASS_64 || bytes[5] != ELF_DATA_LSB || bytes[6] != 1 {
            return Err(ElfError::UnsupportedLayout);
        }
        let e_type = u16_at(bytes, 16)?;
        let machine = Machine::from_e_machine(u16_at(bytes, 18)?)?;
        if u32_at(bytes, 20)? != 1 {
            return Err(ElfError::UnsupportedLayout);
        }
        let entry = u64_at(bytes, 24)?;
        let phoff = u64_at(bytes, 32)?;
        let shoff = u64_at(bytes, 40)?;
        let flags = u32_at(bytes, 48)?;
        if usize::from(u16_at(bytes, 52)?) != ELF_HEADER_LEN {
            return Err(ElfError::BadEntrySize);
        }
        let phentsize = u16_at(bytes, 54)?;
        let phnum = u16_at(bytes, 56)?;
        let shentsize = u16_at(bytes, 58)?;
        let shnum = u16_at(bytes, 60)?;
        let shstrndx = u16_at(bytes, 62)?;

        // PN_XNUM escape: the real count lives in section 0. Refused
        // rather than half-decoded; no Tier-1 binary needs 65535 program
        // headers.
        if phnum == 0xFFFF {
            return Err(ElfError::UnsupportedExtension);
        }
        if phnum > 0 {
            if usize::from(phentsize) != PROGRAM_HEADER_LEN {
                return Err(ElfError::BadEntrySize);
            }
            if usize::from(phnum) > MAX_PROGRAM_HEADERS {
                return Err(ElfError::TableTooLarge);
            }
            let table_len = u64::from(phnum) * PROGRAM_HEADER_LEN as u64;
            if !extent_in_file(phoff, table_len, bytes.len()) {
                return Err(ElfError::OutOfBounds);
            }
        }

        // SHN_LORESERVE escapes: `e_shnum == 0` with a section table
        // present, or `e_shstrndx == SHN_XINDEX`, put the real values in
        // section 0. Refused the same way.
        if shnum == 0 && shoff != 0 {
            return Err(ElfError::UnsupportedExtension);
        }
        if shstrndx >= 0xFF00 {
            return Err(ElfError::UnsupportedExtension);
        }
        if shnum > 0 {
            if usize::from(shentsize) != SECTION_HEADER_LEN {
                return Err(ElfError::BadEntrySize);
            }
            if usize::from(shnum) > MAX_SECTIONS {
                return Err(ElfError::TableTooLarge);
            }
            let table_len = u64::from(shnum) * SECTION_HEADER_LEN as u64;
            if !extent_in_file(shoff, table_len, bytes.len()) {
                return Err(ElfError::OutOfBounds);
            }
            if shstrndx != 0 && shstrndx >= shnum {
                return Err(ElfError::BadIndex);
            }
        } else if shstrndx != 0 {
            return Err(ElfError::BadIndex);
        }

        Ok(Self {
            bytes,
            header: ElfHeader {
                e_type,
                machine,
                entry,
                flags,
                phoff,
                shoff,
                phnum,
                shnum,
                shstrndx,
            },
        })
    }

    /// The decoded file header.
    #[must_use]
    pub fn header(&self) -> &ElfHeader {
        &self.header
    }

    /// Decode program header `index`.
    ///
    /// # Errors
    ///
    /// [`ElfError::BadIndex`] if `index >= phnum`.
    pub fn program_header(&self, index: u16) -> Result<ProgramHeader, ElfError> {
        if index >= self.header.phnum {
            return Err(ElfError::BadIndex);
        }
        let base = self.header.phoff + u64::from(index) * PROGRAM_HEADER_LEN as u64;
        let entry = file_slice(self.bytes, base, PROGRAM_HEADER_LEN as u64)?;
        Ok(ProgramHeader {
            p_type: u32_at(entry, 0)?,
            flags: u32_at(entry, 4)?,
            offset: u64_at(entry, 8)?,
            vaddr: u64_at(entry, 16)?,
            paddr: u64_at(entry, 24)?,
            file_size: u64_at(entry, 32)?,
            mem_size: u64_at(entry, 40)?,
            align: u64_at(entry, 48)?,
        })
    }

    /// Decode section header `index`.
    ///
    /// # Errors
    ///
    /// [`ElfError::BadIndex`] if `index >= shnum`.
    pub fn section(&self, index: u16) -> Result<Section, ElfError> {
        if index >= self.header.shnum {
            return Err(ElfError::BadIndex);
        }
        let base = self.header.shoff + u64::from(index) * SECTION_HEADER_LEN as u64;
        let entry = file_slice(self.bytes, base, SECTION_HEADER_LEN as u64)?;
        Ok(Section {
            name_offset: u32_at(entry, 0)?,
            sh_type: u32_at(entry, 4)?,
            flags: u64_at(entry, 8)?,
            addr: u64_at(entry, 16)?,
            offset: u64_at(entry, 24)?,
            size: u64_at(entry, 32)?,
            link: u32_at(entry, 40)?,
            info: u32_at(entry, 44)?,
            align: u64_at(entry, 48)?,
            entsize: u64_at(entry, 56)?,
        })
    }

    /// Resolve a section's name from the section-name string table.
    ///
    /// # Errors
    ///
    /// [`ElfError::BadIndex`] if the file names no string table;
    /// [`ElfError::BadString`] for an out-of-bounds, unterminated, or
    /// non-UTF-8 name.
    pub fn section_name(&self, section: &Section) -> Result<&'a str, ElfError> {
        if self.header.shstrndx == 0 {
            return Err(ElfError::BadIndex);
        }
        let strtab = self.section(self.header.shstrndx)?;
        if strtab.sh_type != SHT_STRTAB {
            return Err(ElfError::WrongSectionType);
        }
        let table = file_slice(self.bytes, strtab.offset, strtab.size)?;
        read_string(table, section.name_offset)
    }

    /// The file bytes a section occupies.
    ///
    /// # Errors
    ///
    /// [`ElfError::WrongSectionType`] for a `SHT_NOBITS` section (it has
    /// no file bytes); [`ElfError::OutOfBounds`] for an extent outside
    /// the file.
    pub fn section_bytes(&self, section: &Section) -> Result<&'a [u8], ElfError> {
        if section.sh_type == SHT_NOBITS {
            return Err(ElfError::WrongSectionType);
        }
        file_slice(self.bytes, section.offset, section.size)
    }

    /// Open the symbol table held by section `index` (`SHT_SYMTAB` or
    /// `SHT_DYNSYM`), with its linked string table.
    ///
    /// # Errors
    ///
    /// [`ElfError::WrongSectionType`] if the section is not a symbol
    /// table or its `sh_link` names no string table;
    /// [`ElfError::BadEntrySize`] if `sh_entsize` disagrees with ELF64 or
    /// `sh_size` is not a whole number of entries;
    /// [`ElfError::TableTooLarge`] beyond [`MAX_SYMBOLS`].
    pub fn symbol_table(&self, index: u16) -> Result<SymbolTable<'a>, ElfError> {
        let section = self.section(index)?;
        if section.sh_type != SHT_SYMTAB && section.sh_type != SHT_DYNSYM {
            return Err(ElfError::WrongSectionType);
        }
        if section.entsize != SYMBOL_LEN as u64 || section.size % SYMBOL_LEN as u64 != 0 {
            return Err(ElfError::BadEntrySize);
        }
        let count = section.size / SYMBOL_LEN as u64;
        if count > MAX_SYMBOLS as u64 {
            return Err(ElfError::TableTooLarge);
        }
        let entries = file_slice(self.bytes, section.offset, section.size)?;
        let link = u16::try_from(section.link).map_err(|_| ElfError::BadIndex)?;
        let strtab_section = self.section(link)?;
        if strtab_section.sh_type != SHT_STRTAB {
            return Err(ElfError::WrongSectionType);
        }
        let strtab = file_slice(self.bytes, strtab_section.offset, strtab_section.size)?;
        Ok(SymbolTable { entries, strtab })
    }
}

/// A validated symbol table plus its linked string table; entries decode
/// on access.
#[derive(Copy, Clone, Debug)]
pub struct SymbolTable<'a> {
    entries: &'a [u8],
    strtab: &'a [u8],
}

impl<'a> SymbolTable<'a> {
    /// Number of entries (including the mandatory null entry 0).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len() / SYMBOL_LEN
    }

    /// Whether the table holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Decode symbol `index`.
    ///
    /// # Errors
    ///
    /// [`ElfError::BadIndex`] if `index >= len`.
    pub fn symbol(&self, index: usize) -> Result<Symbol, ElfError> {
        let base = index.checked_mul(SYMBOL_LEN).ok_or(ElfError::BadIndex)?;
        let end = base.checked_add(SYMBOL_LEN).ok_or(ElfError::BadIndex)?;
        let entry = self.entries.get(base..end).ok_or(ElfError::BadIndex)?;
        Ok(Symbol {
            name_offset: u32_at(entry, 0)?,
            info: entry[4],
            other: entry[5],
            shndx: u16_at(entry, 6)?,
            value: u64_at(entry, 8)?,
            size: u64_at(entry, 16)?,
        })
    }

    /// Resolve a symbol's name from the linked string table.
    ///
    /// # Errors
    ///
    /// [`ElfError::BadString`] for an out-of-bounds, unterminated, or
    /// non-UTF-8 name.
    pub fn name(&self, symbol: &Symbol) -> Result<&'a str, ElfError> {
        read_string(self.strtab, symbol.name_offset)
    }
}

#[cfg(test)]
#[path = "elf_tests.rs"]
mod tests;
