//! Behavioural tests for the ELF64 view: a hand-assembled fixture file, the
//! success paths, and the truncation/mutation refusal matrix.

use super::{
    ElfError, ElfView, Machine, Section, ELF_HEADER_LEN, MAX_PROGRAM_HEADERS, MAX_SECTIONS,
    PROGRAM_HEADER_LEN, SECTION_HEADER_LEN, SHT_NOBITS, SHT_STRTAB, SHT_SYMTAB, SYMBOL_LEN,
};
use alloc::vec::Vec;

const TEXT: &[u8] = &[0x90, 0x90, 0x90, 0x90, 0xC3, 0x00, 0x00, 0x00];
const STRTAB: &[u8] = b"\0main\0";
const SHSTRTAB: &[u8] = b"\0.text\0.symtab\0.strtab\0.shstrtab\0";
const ENTRY: u64 = 0x40_1000;

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Encode one section header from the decoded [`Section`] shape, so the
/// fixture round-trips the exact struct the accessor returns.
fn push_section(out: &mut Vec<u8>, s: &Section) {
    push_u32(out, s.name_offset);
    push_u32(out, s.sh_type);
    push_u64(out, s.flags);
    push_u64(out, s.addr);
    push_u64(out, s.offset);
    push_u64(out, s.size);
    push_u32(out, s.link);
    push_u32(out, s.info);
    push_u64(out, s.align);
    push_u64(out, s.entsize);
}

/// Append the fixture's five-entry section table: null, `.text`,
/// `.symtab`, `.strtab`, `.shstrtab`.
fn push_fixture_sections(out: &mut Vec<u8>, text_off: u64, strtab_off: u64, shstrtab_off: u64) {
    let symtab_off = shstrtab_off + SHSTRTAB.len() as u64;
    push_section(
        out,
        &Section {
            name_offset: 0,
            sh_type: 0,
            flags: 0,
            addr: 0,
            offset: 0,
            size: 0,
            link: 0,
            info: 0,
            align: 0,
            entsize: 0,
        },
    );
    push_section(
        out,
        &Section {
            name_offset: 1, // ".text"
            sh_type: 1,     // SHT_PROGBITS
            flags: 0b110,
            addr: ENTRY,
            offset: text_off,
            size: TEXT.len() as u64,
            link: 0,
            info: 0,
            align: 16,
            entsize: 0,
        },
    );
    push_section(
        out,
        &Section {
            name_offset: 7, // ".symtab"
            sh_type: SHT_SYMTAB,
            flags: 0,
            addr: 0,
            offset: symtab_off,
            size: 2 * SYMBOL_LEN as u64,
            link: 3, // sh_link -> .strtab
            info: 1,
            align: 8,
            entsize: SYMBOL_LEN as u64,
        },
    );
    push_section(
        out,
        &Section {
            name_offset: 15, // ".strtab"
            sh_type: SHT_STRTAB,
            flags: 0,
            addr: 0,
            offset: strtab_off,
            size: STRTAB.len() as u64,
            link: 0,
            info: 0,
            align: 1,
            entsize: 0,
        },
    );
    push_section(
        out,
        &Section {
            name_offset: 23, // ".shstrtab"
            sh_type: SHT_STRTAB,
            flags: 0,
            addr: 0,
            offset: shstrtab_off,
            size: SHSTRTAB.len() as u64,
            link: 0,
            info: 0,
            align: 1,
            entsize: 0,
        },
    );
}

/// Build a minimal valid ELF64 x86_64 executable: one `PT_LOAD` program
/// header, then `.text` bytes, `.strtab`, `.shstrtab`, a two-entry
/// `.symtab` (null + `main`), and the section table last (so every
/// truncation of the file breaks a validated extent).
fn fixture() -> Vec<u8> {
    let phoff = ELF_HEADER_LEN as u64;
    let text_off = phoff + PROGRAM_HEADER_LEN as u64;
    let strtab_off = text_off + TEXT.len() as u64;
    let shstrtab_off = strtab_off + STRTAB.len() as u64;
    let symtab_off = shstrtab_off + SHSTRTAB.len() as u64;
    let symtab_len = 2 * SYMBOL_LEN as u64;
    let shoff = symtab_off + symtab_len;

    let mut out = Vec::new();
    // File header.
    out.extend_from_slice(b"\x7fELF");
    out.extend_from_slice(&[2, 1, 1, 0]); // 64-bit, LSB, ident version 1.
    out.extend_from_slice(&[0; 8]); // padding
    push_u16(&mut out, 2); // e_type: executable
    push_u16(&mut out, 62); // e_machine: EM_X86_64
    push_u32(&mut out, 1); // e_version
    push_u64(&mut out, ENTRY);
    push_u64(&mut out, phoff);
    push_u64(&mut out, shoff);
    push_u32(&mut out, 0); // e_flags
    push_u16(&mut out, u16::try_from(ELF_HEADER_LEN).expect("fits"));
    push_u16(&mut out, u16::try_from(PROGRAM_HEADER_LEN).expect("fits"));
    push_u16(&mut out, 1); // e_phnum
    push_u16(&mut out, u16::try_from(SECTION_HEADER_LEN).expect("fits"));
    push_u16(&mut out, 5); // e_shnum
    push_u16(&mut out, 4); // e_shstrndx
    assert_eq!(out.len(), ELF_HEADER_LEN);

    // Program header: PT_LOAD, R+X, covering .text.
    push_u32(&mut out, 1); // p_type: PT_LOAD
    push_u32(&mut out, 0b101); // p_flags: R + X
    push_u64(&mut out, text_off); // p_offset
    push_u64(&mut out, ENTRY); // p_vaddr
    push_u64(&mut out, ENTRY); // p_paddr
    push_u64(&mut out, TEXT.len() as u64);
    push_u64(&mut out, TEXT.len() as u64);
    push_u64(&mut out, 0x1000); // p_align

    out.extend_from_slice(TEXT);
    out.extend_from_slice(STRTAB);
    out.extend_from_slice(SHSTRTAB);

    // Symbol 0: the mandatory null entry.
    out.extend_from_slice(&[0; SYMBOL_LEN]);
    // Symbol 1: `main` at the entry point, defined in section 1.
    push_u32(&mut out, 1); // st_name -> "main"
    out.push(0x12); // st_info: GLOBAL FUNC
    out.push(0); // st_other
    push_u16(&mut out, 1); // st_shndx -> .text
    push_u64(&mut out, ENTRY);
    push_u64(&mut out, TEXT.len() as u64);

    // Section table: null, .text, .symtab, .strtab, .shstrtab.
    assert_eq!(out.len() as u64, shoff);
    push_fixture_sections(&mut out, text_off, strtab_off, shstrtab_off);
    out
}

/// A minimal 64-byte header-only file with no tables, mutated by the
/// refusal tests that need full control of the header fields.
fn header_only() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"\x7fELF");
    out.extend_from_slice(&[2, 1, 1, 0]);
    out.extend_from_slice(&[0; 8]);
    push_u16(&mut out, 2);
    push_u16(&mut out, 62);
    push_u32(&mut out, 1);
    push_u64(&mut out, 0); // e_entry
    push_u64(&mut out, 0); // e_phoff
    push_u64(&mut out, 0); // e_shoff
    push_u32(&mut out, 0);
    push_u16(&mut out, u16::try_from(ELF_HEADER_LEN).expect("fits"));
    push_u16(&mut out, 0); // e_phentsize
    push_u16(&mut out, 0); // e_phnum
    push_u16(&mut out, 0); // e_shentsize
    push_u16(&mut out, 0); // e_shnum
    push_u16(&mut out, 0); // e_shstrndx
    assert_eq!(out.len(), ELF_HEADER_LEN);
    out
}

fn set_u16(bytes: &mut [u8], offset: usize, v: u16) {
    bytes[offset..offset + 2].copy_from_slice(&v.to_le_bytes());
}

fn set_u64(bytes: &mut [u8], offset: usize, v: u64) {
    bytes[offset..offset + 8].copy_from_slice(&v.to_le_bytes());
}

#[test]
fn parses_the_fixture_header() {
    let bytes = fixture();
    let view = ElfView::parse(&bytes).expect("valid ELF");
    let header = view.header();
    assert_eq!(header.e_type, 2);
    assert_eq!(header.machine, Machine::X86_64);
    assert_eq!(header.machine.name(), "x86_64");
    assert_eq!(header.entry, ENTRY);
    assert_eq!(header.phnum, 1);
    assert_eq!(header.shnum, 5);
    assert_eq!(header.shstrndx, 4);
}

#[test]
fn decodes_the_program_header_and_refuses_a_bad_index() {
    let bytes = fixture();
    let view = ElfView::parse(&bytes).expect("valid ELF");
    let ph = view.program_header(0).expect("phdr 0");
    assert_eq!(ph.p_type, 1);
    assert_eq!(ph.flags, 0b101);
    assert_eq!(ph.vaddr, ENTRY);
    assert_eq!(ph.file_size, TEXT.len() as u64);
    assert_eq!(view.program_header(1), Err(ElfError::BadIndex));
}

#[test]
fn resolves_section_names_and_bytes() {
    let bytes = fixture();
    let view = ElfView::parse(&bytes).expect("valid ELF");
    let names: Vec<&str> = (0..view.header().shnum)
        .map(|i| {
            let section = view.section(i).expect("section");
            view.section_name(&section).expect("name")
        })
        .collect();
    assert_eq!(names, ["", ".text", ".symtab", ".strtab", ".shstrtab"]);

    let text = view.section(1).expect("section 1");
    assert_eq!(view.section_bytes(&text).expect("bytes"), TEXT);
    assert_eq!(view.section(5), Err(ElfError::BadIndex));
}

#[test]
fn nobits_sections_expose_no_file_bytes() {
    let mut bytes = fixture();
    let shoff = ElfView::parse(&bytes).expect("valid ELF").header().shoff;
    // Turn .text into SHT_NOBITS (sh_type is 4 bytes into entry 1).
    let type_off = usize::try_from(shoff).expect("fits") + SECTION_HEADER_LEN + 4;
    bytes[type_off..type_off + 4].copy_from_slice(&SHT_NOBITS.to_le_bytes());
    let view = ElfView::parse(&bytes).expect("still valid");
    let text = view.section(1).expect("section 1");
    assert_eq!(view.section_bytes(&text), Err(ElfError::WrongSectionType));
}

#[test]
fn reads_symbols_and_their_names() {
    let bytes = fixture();
    let view = ElfView::parse(&bytes).expect("valid ELF");
    let table = view.symbol_table(2).expect("symtab");
    assert_eq!(table.len(), 2);
    assert!(!table.is_empty());
    let sym = table.symbol(1).expect("main symbol");
    assert_eq!(sym.value, ENTRY);
    assert_eq!(sym.size, TEXT.len() as u64);
    assert_eq!(sym.shndx, 1);
    assert_eq!(table.name(&sym).expect("name"), "main");
    assert_eq!(table.symbol(2), Err(ElfError::BadIndex));
}

#[test]
fn refuses_a_symbol_table_request_on_a_non_symbol_section() {
    let bytes = fixture();
    let view = ElfView::parse(&bytes).expect("valid ELF");
    assert!(matches!(
        view.symbol_table(1),
        Err(ElfError::WrongSectionType)
    ));
}

#[test]
fn refuses_an_out_of_bounds_symbol_name() {
    let bytes = fixture();
    let view = ElfView::parse(&bytes).expect("valid ELF");
    let table = view.symbol_table(2).expect("symtab");
    let mut sym = table.symbol(1).expect("main symbol");
    sym.name_offset = u32::try_from(STRTAB.len()).expect("fits");
    assert_eq!(table.name(&sym), Err(ElfError::BadString));
}

#[test]
fn every_truncation_fails_closed() {
    let bytes = fixture();
    for len in 0..bytes.len() {
        assert!(
            ElfView::parse(&bytes[..len]).is_err(),
            "truncation to {len} bytes must be refused"
        );
    }
}

#[test]
fn identification_mutations_are_refused() {
    let good = fixture();

    let mut bad_magic = good.clone();
    bad_magic[0] = b'?';
    assert!(matches!(
        ElfView::parse(&bad_magic),
        Err(ElfError::BadMagic)
    ));

    let mut elf32 = good.clone();
    elf32[4] = 1;
    assert!(matches!(
        ElfView::parse(&elf32),
        Err(ElfError::UnsupportedLayout)
    ));

    let mut big_endian = good.clone();
    big_endian[5] = 2;
    assert!(matches!(
        ElfView::parse(&big_endian),
        Err(ElfError::UnsupportedLayout)
    ));

    let mut bad_ident_version = good.clone();
    bad_ident_version[6] = 0;
    assert!(matches!(
        ElfView::parse(&bad_ident_version),
        Err(ElfError::UnsupportedLayout)
    ));

    let mut bad_version = good.clone();
    bad_version[20] = 2;
    assert!(matches!(
        ElfView::parse(&bad_version),
        Err(ElfError::UnsupportedLayout)
    ));

    let mut alien_machine = good;
    set_u16(&mut alien_machine, 18, 40); // EM_ARM (32-bit)
    assert!(matches!(
        ElfView::parse(&alien_machine),
        Err(ElfError::UnsupportedMachine(40))
    ));
}

#[test]
fn size_and_extension_mutations_are_refused() {
    let good = fixture();

    let mut bad_ehsize = good.clone();
    set_u16(&mut bad_ehsize, 52, 32);
    assert!(matches!(
        ElfView::parse(&bad_ehsize),
        Err(ElfError::BadEntrySize)
    ));

    let mut wrong_phdr_size = good.clone();
    set_u16(&mut wrong_phdr_size, 54, 32);
    assert!(matches!(
        ElfView::parse(&wrong_phdr_size),
        Err(ElfError::BadEntrySize)
    ));

    let mut wrong_section_size = good.clone();
    set_u16(&mut wrong_section_size, 58, 32);
    assert!(matches!(
        ElfView::parse(&wrong_section_size),
        Err(ElfError::BadEntrySize)
    ));

    let mut pn_xnum = good.clone();
    set_u16(&mut pn_xnum, 56, 0xFFFF);
    assert!(matches!(
        ElfView::parse(&pn_xnum),
        Err(ElfError::UnsupportedExtension)
    ));

    let mut shnum_escape = good.clone();
    set_u16(&mut shnum_escape, 60, 0); // shnum 0 with shoff != 0
    assert!(matches!(
        ElfView::parse(&shnum_escape),
        Err(ElfError::UnsupportedExtension)
    ));

    let mut shstrndx_escape = good.clone();
    set_u16(&mut shstrndx_escape, 62, 0xFF00);
    assert!(matches!(
        ElfView::parse(&shstrndx_escape),
        Err(ElfError::UnsupportedExtension)
    ));

    let mut shstrndx_oob = good;
    set_u16(&mut shstrndx_oob, 62, 5);
    assert!(matches!(
        ElfView::parse(&shstrndx_oob),
        Err(ElfError::BadIndex)
    ));
}

#[test]
fn out_of_file_tables_are_refused() {
    let good = fixture();

    let mut wild_phdr_table = good.clone();
    set_u64(&mut wild_phdr_table, 32, u64::MAX - 8);
    assert!(matches!(
        ElfView::parse(&wild_phdr_table),
        Err(ElfError::OutOfBounds)
    ));

    let mut wild_section_table = good;
    set_u64(&mut wild_section_table, 40, u64::MAX - 8);
    assert!(matches!(
        ElfView::parse(&wild_section_table),
        Err(ElfError::OutOfBounds)
    ));
}

#[test]
fn table_caps_are_enforced() {
    let mut many_phdrs = header_only();
    set_u16(
        &mut many_phdrs,
        54,
        u16::try_from(PROGRAM_HEADER_LEN).expect("fits"),
    );
    set_u16(
        &mut many_phdrs,
        56,
        u16::try_from(MAX_PROGRAM_HEADERS + 1).expect("fits"),
    );
    assert!(matches!(
        ElfView::parse(&many_phdrs),
        Err(ElfError::TableTooLarge)
    ));

    let mut many_sections = header_only();
    set_u64(&mut many_sections, 40, 1); // shoff != 0 so shnum > 0 required
    set_u16(
        &mut many_sections,
        58,
        u16::try_from(SECTION_HEADER_LEN).expect("fits"),
    );
    set_u16(
        &mut many_sections,
        60,
        u16::try_from(MAX_SECTIONS + 1).expect("fits"),
    );
    assert!(matches!(
        ElfView::parse(&many_sections),
        Err(ElfError::TableTooLarge)
    ));
}

#[test]
fn a_headers_only_file_parses_with_no_tables() {
    let bytes = header_only();
    let view = ElfView::parse(&bytes).expect("valid header-only ELF");
    assert_eq!(view.header().phnum, 0);
    assert_eq!(view.header().shnum, 0);
    assert_eq!(view.program_header(0), Err(ElfError::BadIndex));
    assert_eq!(view.section(0), Err(ElfError::BadIndex));
}

/// Walk everything a view exposes, ignoring per-item errors; used by the
/// flip sweep to prove no access path can panic on damaged input.
fn exercise(view: &ElfView<'_>) {
    for i in 0..view.header().phnum {
        let _ = view.program_header(i);
    }
    for i in 0..view.header().shnum {
        if let Ok(section) = view.section(i) {
            let _ = view.section_name(&section);
            let _ = view.section_bytes(&section);
        }
        if let Ok(table) = view.symbol_table(i) {
            for s in 0..table.len() {
                if let Ok(sym) = table.symbol(s) {
                    let _ = table.name(&sym);
                }
            }
        }
    }
}

#[test]
fn single_byte_flips_never_panic() {
    let good = fixture();
    for i in 0..good.len() {
        let mut mutated = good.clone();
        mutated[i] ^= 0x40;
        if let Ok(view) = ElfView::parse(&mutated) {
            exercise(&view);
        }
    }
}
