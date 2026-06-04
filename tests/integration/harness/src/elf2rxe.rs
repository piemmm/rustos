//! Build-time ELF -> `rxe` converter for the CCOMPAT spawn round-trips.
//!
//! The CC3 fixture program (`tests/integration/cc3_program`) is compiled as a
//! separate, position-independent C-ABI executable (crt0 + `ros_sys_*` stubs).
//! The kernel-side spawn path consumes an [`rustos_abi::rxe`] load image, not a
//! raw ELF, so the consuming test's build script converts the freshly linked
//! program ELF into an `rxe` blob with [`elf_to_rxe`] and embeds the bytes.
//!
//! The converter is deliberately strict (`AGENTS.md` §5.4 — fail closed). It
//! accepts only what the §19.2 hardening invariants and the fixture's link
//! recipe (`tests/integration/cc3_program/program.ld`) produce:
//!
//! * a little-endian ELF64 **`ET_DYN`** (PIE) image for one of the three native
//!   Tier-1 machines;
//! * `PT_LOAD` segments that are page-aligned and W^X-clean (no segment is both
//!   writable and executable);
//! * dynamic relocations that are **exclusively** the architecture's
//!   `R_*_RELATIVE` form. Any symbolic, GOT, PLT, or `REL`-form relocation is
//!   rejected — the fixture links with none, and accepting them would mean
//!   running a userland dynamic linker the kernel spawn path does not have.
//!
//! Relocations are applied for a **zero load bias**: the fixture is mapped at
//! its link addresses (see `program.ld`), so each `R_*_RELATIVE` target is
//! patched to its addend. The emitted image still declares
//! [`rustos_abi::rxe::LOAD_FLAG_PIE`]; baking the relocations in for zero bias
//! is what lets the kernel map the validated image directly without a runtime
//! relocator, while the load-time policy in [`rustos_abi::rxe::LoadImage`]
//! still enforces every invariant.
//!
//! The `rxe` wire format itself is never re-encoded here: the header and
//! segment records come from [`rustos_abi::rxe`]'s own encoders (§2.2).

use rustos_abi::rxe::{
    LoadHeader, RxePermission, Segment, LOAD_FLAG_PIE, LOAD_MAGIC, LOAD_MAX_SEGMENTS, RXE_PAGE_SIZE,
};
use rustos_abi::syscall::SYSCALL_TABLE_HASH_LEN;
use rustos_abi::ABI_VERSION_CURRENT;

/// Why an ELF image could not be converted to an `rxe` load image.
///
/// Every variant is a hard refusal: the converter never silently drops or
/// guesses at malformed or unsupported input (`AGENTS.md` §5.4).
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Elf2RxeError {
    /// The buffer is shorter than a structure the parse must read.
    Truncated,
    /// The ELF identification bytes are not `0x7F "ELF"`.
    NotElf,
    /// The image is not little-endian ELF64 (the only class the native
    /// Tier-1 targets use).
    NotElf64Le,
    /// The image is not `ET_DYN`; §19.2 requires a position-independent
    /// executable.
    NotPositionIndependent,
    /// `e_machine` is not one of the three native Tier-1 machines.
    UnsupportedMachine,
    /// A program-header field is malformed (e.g. `p_filesz > p_memsz`, or a
    /// non-page-aligned `PT_LOAD` virtual address).
    BadProgramHeader,
    /// A `PT_LOAD` segment is both writable and executable, violating W^X.
    WriteExecSegment,
    /// The image declares more loadable segments than [`LOAD_MAX_SEGMENTS`].
    TooManySegments,
    /// The image declares no loadable segments.
    NoSegments,
    /// The dynamic section uses an unsupported relocation form (anything
    /// other than the architecture's `R_*_RELATIVE`), or carries a `REL` /
    /// `PLT` relocation table.
    UnsupportedRelocation,
    /// A relocation targets an address outside any file-backed `PT_LOAD`
    /// region.
    RelocationOutOfRange,
    /// An offset or size computation overflowed.
    Overflow,
}

impl core::fmt::Display for Elf2RxeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let message = match self {
            Self::Truncated => "elf image truncated",
            Self::NotElf => "not an elf image",
            Self::NotElf64Le => "not little-endian elf64",
            Self::NotPositionIndependent => "elf image is not ET_DYN (PIE)",
            Self::UnsupportedMachine => "unsupported elf machine",
            Self::BadProgramHeader => "malformed program header",
            Self::WriteExecSegment => "segment is writable and executable",
            Self::TooManySegments => "too many loadable segments",
            Self::NoSegments => "no loadable segments",
            Self::UnsupportedRelocation => "unsupported relocation form",
            Self::RelocationOutOfRange => "relocation target out of range",
            Self::Overflow => "offset or size computation overflowed",
        };
        f.write_str(message)
    }
}

impl std::error::Error for Elf2RxeError {}

// --- ELF constants (little-endian ELF64) --------------------------------

const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ET_DYN: u16 = 3;

const EM_X86_64: u16 = 62;
const EM_AARCH64: u16 = 183;
const EM_RISCV: u16 = 243;

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;

const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

const DT_NULL: u64 = 0;
const DT_REL: u64 = 17;
const DT_RELSZ: u64 = 18;
const DT_RELA: u64 = 7;
const DT_RELASZ: u64 = 8;
const DT_RELAENT: u64 = 9;
const DT_JMPREL: u64 = 23;
const DT_PLTRELSZ: u64 = 2;

const R_X86_64_RELATIVE: u32 = 8;
const R_AARCH64_RELATIVE: u32 = 1027;
const R_RISCV_RELATIVE: u32 = 3;

const EHDR_LEN: usize = 64;
const PHDR_LEN: usize = 56;
const DYN_LEN: usize = 16;
const RELA_LEN: usize = 24;

/// One loadable segment recovered from the ELF, with its file-backed bytes
/// copied out so relocations can be applied in place before encoding.
struct LoadSeg {
    /// Virtual address the segment is mapped at (page-aligned).
    vaddr: u64,
    /// Offset of the segment's file-backed bytes in the *original* ELF, kept
    /// so a relocation's target virtual address can be located in the source
    /// file when reading the dynamic and relocation tables.
    elf_offset: u64,
    /// Total in-memory size; bytes beyond `bytes.len()` are zero-filled.
    mem_size: u64,
    /// W^X-clean permission this segment is mapped with.
    permission: RxePermission,
    /// The segment's file-backed bytes, patched in place by relocations.
    bytes: Vec<u8>,
}

/// Convert a linked PIE ELF image into an `rxe` load-image blob.
///
/// `cfi_tag` is the syscall-interface hash the emitted image declares; it must
/// match the kernel's compiled-in hash, or [`rustos_abi::rxe::LoadImage::parse`]
/// will reject the image (§9 / §19.2). Relocations are baked in for a zero load
/// bias (the image is mapped at its link addresses).
///
/// # Errors
///
/// Returns the first [`Elf2RxeError`] encountered. The conversion is total and
/// panic-free over arbitrary input.
pub fn elf_to_rxe(
    elf: &[u8],
    cfi_tag: &[u8; SYSCALL_TABLE_HASH_LEN],
) -> Result<Vec<u8>, Elf2RxeError> {
    let machine = parse_identification(elf)?;
    let entry = read_u64(elf, 24)?;
    let phoff = usize_of(read_u64(elf, 32)?)?;
    let phentsize = usize::from(read_u16(elf, 54)?);
    let phnum = usize::from(read_u16(elf, 56)?);
    if phentsize != PHDR_LEN {
        return Err(Elf2RxeError::BadProgramHeader);
    }

    let mut loads: Vec<LoadSeg> = Vec::new();
    let mut dynamic: Option<(u64, u64)> = None;
    for i in 0..phnum {
        let base = phoff
            .checked_add(i.checked_mul(PHDR_LEN).ok_or(Elf2RxeError::Overflow)?)
            .ok_or(Elf2RxeError::Overflow)?;
        let phdr = elf
            .get(base..base + PHDR_LEN)
            .ok_or(Elf2RxeError::Truncated)?;
        let p_type = read_u32(phdr, 0)?;
        match p_type {
            PT_LOAD => loads.push(decode_load(elf, phdr)?),
            PT_DYNAMIC => {
                let p_vaddr = read_u64(phdr, 16)?;
                let p_filesz = read_u64(phdr, 32)?;
                dynamic = Some((p_vaddr, p_filesz));
            }
            _ => {}
        }
    }

    if loads.is_empty() {
        return Err(Elf2RxeError::NoSegments);
    }
    if loads.len() > LOAD_MAX_SEGMENTS {
        return Err(Elf2RxeError::TooManySegments);
    }
    loads.sort_by_key(|s| s.vaddr);

    if let Some((dyn_vaddr, dyn_filesz)) = dynamic {
        apply_relocations(elf, machine, dyn_vaddr, dyn_filesz, &mut loads)?;
    }

    encode_rxe(entry, cfi_tag, &loads)
}

/// Validate the ELF identification bytes and return the target machine.
fn parse_identification(elf: &[u8]) -> Result<u16, Elf2RxeError> {
    if elf.len() < EHDR_LEN {
        return Err(Elf2RxeError::Truncated);
    }
    if elf[0..4] != ELF_MAGIC {
        return Err(Elf2RxeError::NotElf);
    }
    if elf[4] != ELFCLASS64 || elf[5] != ELFDATA2LSB {
        return Err(Elf2RxeError::NotElf64Le);
    }
    if read_u16(elf, 16)? != ET_DYN {
        return Err(Elf2RxeError::NotPositionIndependent);
    }
    let machine = read_u16(elf, 18)?;
    match machine {
        EM_X86_64 | EM_AARCH64 | EM_RISCV => Ok(machine),
        _ => Err(Elf2RxeError::UnsupportedMachine),
    }
}

/// Decode a single `PT_LOAD` program header into a [`LoadSeg`], copying its
/// file-backed bytes and enforcing the page-alignment and W^X invariants.
fn decode_load(elf: &[u8], phdr: &[u8]) -> Result<LoadSeg, Elf2RxeError> {
    let p_flags = read_u32(phdr, 4)?;
    let p_offset_u64 = read_u64(phdr, 8)?;
    let p_offset = usize_of(p_offset_u64)?;
    let p_vaddr = read_u64(phdr, 16)?;
    let p_filesz = usize_of(read_u64(phdr, 32)?)?;
    let p_memsz = read_u64(phdr, 40)?;

    if p_vaddr % RXE_PAGE_SIZE != 0 {
        return Err(Elf2RxeError::BadProgramHeader);
    }
    if u64::try_from(p_filesz).map_err(|_| Elf2RxeError::Overflow)? > p_memsz || p_memsz == 0 {
        return Err(Elf2RxeError::BadProgramHeader);
    }

    if p_flags & PF_R == 0 {
        // Every mapped segment must be readable; the `rxe` policy refuses a
        // non-readable segment, so reject it here too (fail closed).
        return Err(Elf2RxeError::BadProgramHeader);
    }
    let writable = p_flags & PF_W != 0;
    let executable = p_flags & PF_X != 0;
    let permission = if writable && executable {
        return Err(Elf2RxeError::WriteExecSegment);
    } else if executable {
        RxePermission::ReadExecute
    } else if writable {
        RxePermission::ReadWrite
    } else {
        RxePermission::ReadOnly
    };

    let end = p_offset
        .checked_add(p_filesz)
        .ok_or(Elf2RxeError::Overflow)?;
    let bytes = elf
        .get(p_offset..end)
        .ok_or(Elf2RxeError::Truncated)?
        .to_vec();
    Ok(LoadSeg {
        vaddr: p_vaddr,
        elf_offset: p_offset_u64,
        mem_size: p_memsz,
        permission,
        bytes,
    })
}

/// Walk the dynamic section, reject every unsupported relocation form, and
/// patch each `R_*_RELATIVE` target with its addend (zero load bias).
fn apply_relocations(
    elf: &[u8],
    machine: u16,
    dyn_vaddr: u64,
    dyn_filesz: u64,
    loads: &mut [LoadSeg],
) -> Result<(), Elf2RxeError> {
    let dyn_off = vaddr_to_elf_offset(loads, dyn_vaddr, dyn_filesz)?;
    let count = usize_of(dyn_filesz)? / DYN_LEN;

    let mut rela: Option<u64> = None;
    let mut rela_size: u64 = 0;
    let mut rela_ent: u64 = RELA_LEN as u64;
    for i in 0..count {
        let off = dyn_off + i * DYN_LEN;
        let tag = read_u64(elf, off)?;
        let val = read_u64(elf, off + 8)?;
        match tag {
            DT_NULL => break,
            DT_RELA => rela = Some(val),
            DT_RELASZ => rela_size = val,
            DT_RELAENT => rela_ent = val,
            // The fixture links with neither REL-form nor PLT relocations;
            // their presence means an unsupported link recipe (fail closed).
            DT_REL | DT_RELSZ | DT_JMPREL | DT_PLTRELSZ if val != 0 => {
                return Err(Elf2RxeError::UnsupportedRelocation);
            }
            _ => {}
        }
    }

    let Some(rela_vaddr) = rela else {
        // No RELA table: a static PIE with no relocations is acceptable.
        return Ok(());
    };
    if rela_ent != RELA_LEN as u64 || rela_size % rela_ent != 0 {
        return Err(Elf2RxeError::UnsupportedRelocation);
    }
    let rela_off = vaddr_to_elf_offset(loads, rela_vaddr, rela_size)?;
    let entries = usize_of(rela_size)? / RELA_LEN;
    let relative_type = relative_reloc_type(machine);

    let mut patched: Vec<(u64, u64)> = Vec::new();
    for i in 0..entries {
        let off = rela_off + i * RELA_LEN;
        let r_offset = read_u64(elf, off)?;
        let r_info = read_u64(elf, off + 8)?;
        let r_addend = read_u64(elf, off + 16)?;
        let r_type = (r_info & 0xFFFF_FFFF) as u32;
        let r_sym = r_info >> 32;
        if r_type != relative_type || r_sym != 0 {
            return Err(Elf2RxeError::UnsupportedRelocation);
        }
        patched.push((r_offset, r_addend));
    }

    for (r_offset, r_addend) in patched {
        patch_relative(loads, r_offset, r_addend)?;
    }
    Ok(())
}

/// Translate a virtual address range into an offset in the *original* ELF
/// file using the `PT_LOAD` map, requiring the whole range to be file-backed.
/// Used to read the dynamic and relocation tables from the source image.
fn vaddr_to_elf_offset(loads: &[LoadSeg], vaddr: u64, size: u64) -> Result<usize, Elf2RxeError> {
    let end = vaddr.checked_add(size).ok_or(Elf2RxeError::Overflow)?;
    for seg in loads {
        let seg_end = seg
            .vaddr
            .checked_add(seg.bytes.len() as u64)
            .ok_or(Elf2RxeError::Overflow)?;
        if vaddr >= seg.vaddr && end <= seg_end {
            let delta = vaddr - seg.vaddr;
            let file = seg
                .elf_offset
                .checked_add(delta)
                .ok_or(Elf2RxeError::Overflow)?;
            return usize_of(file);
        }
    }
    Err(Elf2RxeError::RelocationOutOfRange)
}

/// The architecture's `R_*_RELATIVE` relocation type number.
fn relative_reloc_type(machine: u16) -> u32 {
    match machine {
        EM_X86_64 => R_X86_64_RELATIVE,
        EM_AARCH64 => R_AARCH64_RELATIVE,
        EM_RISCV => R_RISCV_RELATIVE,
        // `parse_identification` already restricted the machine set.
        _ => u32::MAX,
    }
}

/// Patch a single 8-byte `R_*_RELATIVE` target in the segment whose mapped
/// range contains it, writing the addend (the relocated value at zero bias).
fn patch_relative(loads: &mut [LoadSeg], r_offset: u64, value: u64) -> Result<(), Elf2RxeError> {
    let end = r_offset.checked_add(8).ok_or(Elf2RxeError::Overflow)?;
    for seg in loads.iter_mut() {
        let seg_file_end = seg
            .vaddr
            .checked_add(seg.bytes.len() as u64)
            .ok_or(Elf2RxeError::Overflow)?;
        if r_offset >= seg.vaddr && end <= seg_file_end {
            let at = usize_of(r_offset - seg.vaddr)?;
            seg.bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
            return Ok(());
        }
    }
    Err(Elf2RxeError::RelocationOutOfRange)
}

/// Assemble the validated segments into an `rxe` blob.
fn encode_rxe(
    entry: u64,
    cfi_tag: &[u8; SYSCALL_TABLE_HASH_LEN],
    loads: &[LoadSeg],
) -> Result<Vec<u8>, Elf2RxeError> {
    let count = u16::try_from(loads.len()).map_err(|_| Elf2RxeError::TooManySegments)?;
    let header = LoadHeader {
        magic: LOAD_MAGIC,
        abi_version: ABI_VERSION_CURRENT,
        flags: LOAD_FLAG_PIE,
        segment_count: count,
        reserved0: 0,
        entry,
        cfi_tag: *cfi_tag,
    };

    let table_len = LoadHeader::WIRE_LEN
        .checked_add(loads.len() * Segment::WIRE_LEN)
        .ok_or(Elf2RxeError::Overflow)?;
    let mut payload_offset = u64::try_from(table_len).map_err(|_| Elf2RxeError::Overflow)?;
    let mut table = Vec::with_capacity(table_len);
    table.extend_from_slice(&header.to_le_bytes());
    let mut payload: Vec<u8> = Vec::new();
    for seg in loads {
        let file_size = u64::try_from(seg.bytes.len()).map_err(|_| Elf2RxeError::Overflow)?;
        let segment = Segment {
            vaddr: seg.vaddr,
            file_offset: payload_offset,
            file_size,
            mem_size: seg.mem_size,
            permission: seg.permission,
        };
        table.extend_from_slice(&segment.to_le_bytes());
        payload.extend_from_slice(&seg.bytes);
        payload_offset = payload_offset
            .checked_add(file_size)
            .ok_or(Elf2RxeError::Overflow)?;
    }
    table.extend_from_slice(&payload);
    Ok(table)
}

// --- little-endian readers (bounds-checked, panic-free) -----------------

fn read_u16(bytes: &[u8], at: usize) -> Result<u16, Elf2RxeError> {
    let slice = bytes.get(at..at + 2).ok_or(Elf2RxeError::Truncated)?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, Elf2RxeError> {
    let slice = bytes.get(at..at + 4).ok_or(Elf2RxeError::Truncated)?;
    let mut buf = [0u8; 4];
    buf.copy_from_slice(slice);
    Ok(u32::from_le_bytes(buf))
}

fn read_u64(bytes: &[u8], at: usize) -> Result<u64, Elf2RxeError> {
    let slice = bytes.get(at..at + 8).ok_or(Elf2RxeError::Truncated)?;
    let mut buf = [0u8; 8];
    buf.copy_from_slice(slice);
    Ok(u64::from_le_bytes(buf))
}

fn usize_of(value: u64) -> Result<usize, Elf2RxeError> {
    usize::try_from(value).map_err(|_| Elf2RxeError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_abi::rxe::LoadImage;

    const TAG: [u8; SYSCALL_TABLE_HASH_LEN] = [0x5A; SYSCALL_TABLE_HASH_LEN];

    // File layout of the synthetic image (offsets and the corresponding
    // virtual addresses the program headers declare).
    const PHOFF: usize = 64;
    const CODE_OFF: usize = 0x100;
    const CODE_LEN: usize = 16;
    const DATA_OFF: usize = 0x200;
    const TARGET_AT: usize = 0x00; // within data: relocation target
    const DYN_AT: usize = 0x40; // within data: dynamic table
    const RELA_AT: usize = 0x80; // within data: RELA table
    const DATA_FILESZ: usize = RELA_AT + RELA_LEN;

    const CODE_VADDR: u64 = 0x1000;
    const DATA_VADDR: u64 = 0x2000;
    const DYN_VADDR: u64 = DATA_VADDR + DYN_AT as u64;
    const RELA_VADDR: u64 = DATA_VADDR + RELA_AT as u64;
    const TARGET_VADDR: u64 = DATA_VADDR + TARGET_AT as u64;
    const RELOC_VALUE: u64 = 0x1234;

    fn w16(buf: &mut [u8], at: usize, x: u16) {
        buf[at..at + 2].copy_from_slice(&x.to_le_bytes());
    }
    fn w32(buf: &mut [u8], at: usize, x: u32) {
        buf[at..at + 4].copy_from_slice(&x.to_le_bytes());
    }
    fn w64(buf: &mut [u8], at: usize, x: u64) {
        buf[at..at + 8].copy_from_slice(&x.to_le_bytes());
    }

    /// One ELF64 program-header description for the test builder.
    struct Phdr {
        kind: u32,
        flags: u32,
        offset: u64,
        vaddr: u64,
        filesz: u64,
        memsz: u64,
        align: u64,
    }

    fn write_phdr(buf: &mut [u8], index: usize, p: &Phdr) {
        let at = PHOFF + index * PHDR_LEN;
        w32(buf, at, p.kind);
        w32(buf, at + 4, p.flags);
        w64(buf, at + 8, p.offset);
        w64(buf, at + 16, p.vaddr);
        w64(buf, at + 24, p.vaddr); // p_paddr mirrors p_vaddr
        w64(buf, at + 32, p.filesz);
        w64(buf, at + 40, p.memsz);
        w64(buf, at + 48, p.align);
    }

    fn write_dyn(buf: &mut [u8], at: usize, tag: u64, val: u64) {
        w64(buf, at, tag);
        w64(buf, at + 8, val);
    }

    /// A valid little-endian riscv64 PIE ELF: a code segment (R+X), a data
    /// segment (R+W) holding one `R_RISCV_RELATIVE` target plus the dynamic
    /// and RELA tables, and an entry point inside the code segment.
    fn sample_elf() -> Vec<u8> {
        let mut buf = vec![0u8; DATA_OFF + DATA_FILESZ];

        // ELF identification + header.
        buf[0..4].copy_from_slice(&ELF_MAGIC);
        buf[4] = ELFCLASS64;
        buf[5] = ELFDATA2LSB;
        buf[6] = 1; // EI_VERSION
        w16(&mut buf, 16, ET_DYN);
        w16(&mut buf, 18, EM_RISCV);
        w32(&mut buf, 20, 1);
        w64(&mut buf, 24, CODE_VADDR); // e_entry
        w64(&mut buf, 32, PHOFF as u64); // e_phoff
        w16(&mut buf, 52, u16::try_from(EHDR_LEN).unwrap());
        w16(&mut buf, 54, u16::try_from(PHDR_LEN).unwrap());
        w16(&mut buf, 56, 3); // e_phnum

        // Program headers: code, data, dynamic.
        write_phdr(
            &mut buf,
            0,
            &Phdr {
                kind: PT_LOAD,
                flags: PF_R | PF_X,
                offset: CODE_OFF as u64,
                vaddr: CODE_VADDR,
                filesz: CODE_LEN as u64,
                memsz: RXE_PAGE_SIZE,
                align: RXE_PAGE_SIZE,
            },
        );
        write_phdr(
            &mut buf,
            1,
            &Phdr {
                kind: PT_LOAD,
                flags: PF_R | PF_W,
                offset: DATA_OFF as u64,
                vaddr: DATA_VADDR,
                filesz: DATA_FILESZ as u64,
                memsz: RXE_PAGE_SIZE,
                align: RXE_PAGE_SIZE,
            },
        );
        write_phdr(
            &mut buf,
            2,
            &Phdr {
                kind: PT_DYNAMIC,
                flags: PF_R | PF_W,
                offset: (DATA_OFF + DYN_AT) as u64,
                vaddr: DYN_VADDR,
                filesz: (4 * DYN_LEN) as u64,
                memsz: (4 * DYN_LEN) as u64,
                align: 8,
            },
        );

        // Some code bytes so the entry lands in initialised memory.
        for b in &mut buf[CODE_OFF..CODE_OFF + CODE_LEN] {
            *b = 0x13; // riscv `nop` low byte; content is irrelevant to the test
        }

        // Dynamic table.
        let dyn_off = DATA_OFF + DYN_AT;
        write_dyn(&mut buf, dyn_off, DT_RELA, RELA_VADDR);
        write_dyn(&mut buf, dyn_off + DYN_LEN, DT_RELASZ, RELA_LEN as u64);
        write_dyn(&mut buf, dyn_off + 2 * DYN_LEN, DT_RELAENT, RELA_LEN as u64);
        write_dyn(&mut buf, dyn_off + 3 * DYN_LEN, DT_NULL, 0);

        // One R_RISCV_RELATIVE relocation patching the target slot.
        let rela_off = DATA_OFF + RELA_AT;
        w64(&mut buf, rela_off, TARGET_VADDR); // r_offset
        w64(&mut buf, rela_off + 8, u64::from(R_RISCV_RELATIVE)); // r_info (sym 0)
        w64(&mut buf, rela_off + 16, RELOC_VALUE); // r_addend

        buf
    }

    /// Read the file-backed bytes of the segment with `vaddr` out of an
    /// already-parsed `rxe` blob.
    fn segment_bytes<'a>(rxe: &'a [u8], image: &LoadImage, vaddr: u64) -> &'a [u8] {
        let seg = image
            .segments()
            .iter()
            .find(|s| s.vaddr == vaddr)
            .expect("segment present");
        let off = usize::try_from(seg.file_offset).unwrap();
        let len = usize::try_from(seg.file_size).unwrap();
        &rxe[off..off + len]
    }

    #[test]
    fn converts_a_valid_pie_and_round_trips_through_loadimage() {
        let rxe = elf_to_rxe(&sample_elf(), &TAG).expect("conversion");
        let image = LoadImage::parse(&rxe, &TAG).expect("valid rxe");
        assert_eq!(image.entry(), CODE_VADDR);

        let segs = image.segments();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].vaddr, CODE_VADDR);
        assert_eq!(segs[0].permission, RxePermission::ReadExecute);
        assert_eq!(segs[1].vaddr, DATA_VADDR);
        assert_eq!(segs[1].permission, RxePermission::ReadWrite);
    }

    #[test]
    fn relative_relocation_is_applied_at_zero_bias() {
        let rxe = elf_to_rxe(&sample_elf(), &TAG).expect("conversion");
        let image = LoadImage::parse(&rxe, &TAG).expect("valid rxe");
        let data = segment_bytes(&rxe, &image, DATA_VADDR);
        let patched = u64::from_le_bytes(data[0..8].try_into().unwrap());
        assert_eq!(patched, RELOC_VALUE);
    }

    #[test]
    fn rejects_non_elf() {
        let mut elf = sample_elf();
        elf[1] ^= 0xFF;
        assert_eq!(elf_to_rxe(&elf, &TAG), Err(Elf2RxeError::NotElf));
    }

    #[test]
    fn rejects_non_elf64_le() {
        let mut elf = sample_elf();
        elf[4] = 1; // ELFCLASS32
        assert_eq!(elf_to_rxe(&elf, &TAG), Err(Elf2RxeError::NotElf64Le));
    }

    #[test]
    fn rejects_non_pie() {
        let mut elf = sample_elf();
        w16(&mut elf, 16, 2); // ET_EXEC
        assert_eq!(
            elf_to_rxe(&elf, &TAG),
            Err(Elf2RxeError::NotPositionIndependent)
        );
    }

    #[test]
    fn rejects_unsupported_machine() {
        let mut elf = sample_elf();
        w16(&mut elf, 18, 0);
        assert_eq!(
            elf_to_rxe(&elf, &TAG),
            Err(Elf2RxeError::UnsupportedMachine)
        );
    }

    #[test]
    fn rejects_write_execute_segment() {
        let mut elf = sample_elf();
        // phdr0 flags -> R|W|X.
        w32(&mut elf, PHOFF + 4, PF_R | PF_W | PF_X);
        assert_eq!(elf_to_rxe(&elf, &TAG), Err(Elf2RxeError::WriteExecSegment));
    }

    #[test]
    fn rejects_misaligned_segment_vaddr() {
        let mut elf = sample_elf();
        w64(&mut elf, PHOFF + 16, CODE_VADDR + 1);
        assert_eq!(elf_to_rxe(&elf, &TAG), Err(Elf2RxeError::BadProgramHeader));
    }

    #[test]
    fn rejects_non_relative_relocation() {
        let mut elf = sample_elf();
        // Change the RELA entry's type away from R_RISCV_RELATIVE.
        let rela_off = DATA_OFF + RELA_AT;
        w64(&mut elf, rela_off + 8, 1); // R_RISCV_32, an absolute reloc
        assert_eq!(
            elf_to_rxe(&elf, &TAG),
            Err(Elf2RxeError::UnsupportedRelocation)
        );
    }

    #[test]
    fn rejects_symbolic_relative_relocation() {
        let mut elf = sample_elf();
        let rela_off = DATA_OFF + RELA_AT;
        // A non-zero symbol index with the RELATIVE type is still rejected.
        w64(
            &mut elf,
            rela_off + 8,
            (1u64 << 32) | u64::from(R_RISCV_RELATIVE),
        );
        assert_eq!(
            elf_to_rxe(&elf, &TAG),
            Err(Elf2RxeError::UnsupportedRelocation)
        );
    }

    #[test]
    fn rejects_truncated_image() {
        let elf = sample_elf();
        assert_eq!(elf_to_rxe(&elf[..32], &TAG), Err(Elf2RxeError::Truncated));
    }

    #[test]
    fn rejects_relocation_out_of_range() {
        let mut elf = sample_elf();
        let rela_off = DATA_OFF + RELA_AT;
        // Point the relocation at an address no PT_LOAD covers.
        w64(&mut elf, rela_off, 0x9000_0000);
        assert_eq!(
            elf_to_rxe(&elf, &TAG),
            Err(Elf2RxeError::RelocationOutOfRange)
        );
    }

    #[test]
    fn cfi_tag_mismatch_is_caught_by_loadimage() {
        let rxe = elf_to_rxe(&sample_elf(), &TAG).expect("conversion");
        let mut wrong = TAG;
        wrong[0] ^= 0xFF;
        assert!(LoadImage::parse(&rxe, &wrong).is_err());
    }
}
