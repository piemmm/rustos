//! Host tests for the loader core.
//!
//! Every fixture is a hand-assembled ELF64 image, so the tests exercise the
//! exact bytes a real loader decodes rather than a mock of the decoder.
//! The success paths confirm the segment geometry, entry point, and
//! physical span; the refusal matrix confirms that every malformed,
//! oversized, overlapping, misaligned, or write-executable image is
//! rejected whole.

extern crate std;

use std::vec::Vec;

use super::*;
use tairix_binfmt::elf::Machine;

const EM_X86_64: u16 = 62;
const EM_AARCH64: u16 = 183;

const PT_NOTE: u32 = 4;

/// One program-header entry to assemble into a fixture.
#[derive(Copy, Clone)]
struct Phdr {
    p_type: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    paddr: u64,
    filesz: u64,
    memsz: u64,
    align: u64,
}

impl Phdr {
    /// A readable+executable code segment with matching file/mem sizes.
    fn code(paddr: u64, memsz: u64) -> Self {
        Phdr {
            p_type: PT_LOAD,
            flags: PF_R | PF_X,
            offset: 0,
            vaddr: paddr,
            paddr,
            filesz: memsz,
            memsz,
            align: 1,
        }
    }
}

/// Assemble an ELF64 image from a header shape and a program-header table.
///
/// The program-header table is placed immediately after the 64-byte file
/// header; the file is then padded to cover the highest segment file
/// extent so every declared `p_offset + p_filesz` lies inside the image.
fn build_elf(e_type: u16, machine: u16, entry: u64, phdrs: &[Phdr]) -> Vec<u8> {
    let phnum = u16::try_from(phdrs.len()).expect("fixture program-header count fits u16");
    let phoff: u64 = 64;
    let table_len = phdrs.len() * 56;

    let mut header = Vec::new();
    header.extend_from_slice(b"\x7fELF"); // magic
    header.push(2); // EI_CLASS = ELFCLASS64
    header.push(1); // EI_DATA = ELFDATA2LSB
    header.push(1); // EI_VERSION
    header.push(0); // EI_OSABI
    header.extend_from_slice(&[0u8; 8]); // EI_PAD
    push_u16(&mut header, e_type); // e_type
    push_u16(&mut header, machine); // e_machine
    push_u32(&mut header, 1); // e_version
    push_u64(&mut header, entry); // e_entry
    push_u64(&mut header, phoff); // e_phoff
    push_u64(&mut header, 0); // e_shoff
    push_u32(&mut header, 0); // e_flags
    push_u16(&mut header, 64); // e_ehsize
    push_u16(&mut header, 56); // e_phentsize
    push_u16(&mut header, phnum); // e_phnum
    push_u16(&mut header, 64); // e_shentsize
    push_u16(&mut header, 0); // e_shnum
    push_u16(&mut header, 0); // e_shstrndx
    assert_eq!(header.len(), 64);

    for ph in phdrs {
        push_u32(&mut header, ph.p_type);
        push_u32(&mut header, ph.flags);
        push_u64(&mut header, ph.offset);
        push_u64(&mut header, ph.vaddr);
        push_u64(&mut header, ph.paddr);
        push_u64(&mut header, ph.filesz);
        push_u64(&mut header, ph.memsz);
        push_u64(&mut header, ph.align);
    }
    assert_eq!(header.len(), 64 + table_len);

    // Pad so every segment's file extent is in-bounds.
    let mut needed = header.len() as u64;
    for ph in phdrs {
        needed = needed.max(ph.offset.saturating_add(ph.filesz));
    }
    let needed = usize::try_from(needed).expect("fixture size fits usize");
    if header.len() < needed {
        header.resize(needed, 0);
    }
    header
}

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

#[test]
fn a_single_code_segment_plans_one_segment() {
    let image = build_elf(
        ET_EXEC,
        EM_X86_64,
        0x10_0000,
        &[Phdr::code(0x10_0000, 0x2000)],
    );
    let plan = plan_kernel_load(&image, Machine::X86_64).expect("valid image plans");
    assert_eq!(plan.entry(), 0x10_0000);
    let segs = plan.segments();
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0].phys_dest, 0x10_0000);
    assert_eq!(segs[0].mem_size, 0x2000);
    assert!(segs[0].flags.readable && segs[0].flags.executable);
    assert!(!segs[0].flags.writable);
    assert_eq!(plan.phys_span(), Some((0x10_0000, 0x10_2000)));
}

#[test]
fn a_bss_tail_is_carried_as_mem_larger_than_file() {
    // file 0x800, mem 0x2000 -> 0x1800 zero-filled tail.
    let mut ph = Phdr::code(0x20_0000, 0x2000);
    ph.filesz = 0x800;
    ph.flags = PF_R | PF_W;
    let image = build_elf(ET_EXEC, EM_X86_64, 0x20_0000, &[ph]);
    let plan = plan_kernel_load(&image, Machine::X86_64).expect("valid bss image plans");
    let seg = plan.segments()[0];
    assert_eq!(seg.file_size, 0x800);
    assert_eq!(seg.mem_size, 0x2000);
    assert!(seg.flags.writable && !seg.flags.executable);
}

#[test]
fn multiple_segments_span_the_full_range() {
    let code = Phdr::code(0x10_0000, 0x1000);
    let mut data = Phdr::code(0x10_4000, 0x2000);
    data.flags = PF_R | PF_W;
    let image = build_elf(ET_EXEC, EM_X86_64, 0x10_0000, &[code, data]);
    let plan = plan_kernel_load(&image, Machine::X86_64).expect("valid two-segment image plans");
    assert_eq!(plan.segments().len(), 2);
    assert_eq!(plan.phys_span(), Some((0x10_0000, 0x10_6000)));
}

#[test]
fn non_loadable_segments_are_skipped() {
    let note = Phdr {
        p_type: PT_NOTE,
        flags: PF_R,
        offset: 0,
        vaddr: 0,
        paddr: 0,
        filesz: 0,
        memsz: 0,
        align: 1,
    };
    let code = Phdr::code(0x30_0000, 0x1000);
    let image = build_elf(ET_EXEC, EM_X86_64, 0x30_0000, &[note, code]);
    let plan = plan_kernel_load(&image, Machine::X86_64).expect("note is skipped, code planned");
    assert_eq!(plan.segments().len(), 1);
    assert_eq!(plan.segments()[0].phys_dest, 0x30_0000);
}

#[test]
fn a_relocatable_object_is_refused() {
    const ET_REL: u16 = 1;
    let image = build_elf(ET_REL, EM_X86_64, 0, &[Phdr::code(0x10_0000, 0x1000)]);
    assert_eq!(
        plan_kernel_load(&image, Machine::X86_64),
        Err(LoadError::NotExecutable)
    );
}

#[test]
fn a_foreign_machine_is_refused_with_the_actual_machine() {
    let image = build_elf(
        ET_EXEC,
        EM_X86_64,
        0x10_0000,
        &[Phdr::code(0x10_0000, 0x1000)],
    );
    assert_eq!(
        plan_kernel_load(&image, Machine::Aarch64),
        Err(LoadError::WrongMachine(Machine::X86_64))
    );
}

#[test]
fn the_same_core_serves_a_foreign_arch_when_asked_for_it() {
    let image = build_elf(
        ET_EXEC,
        EM_AARCH64,
        0x40_0000,
        &[Phdr::code(0x40_0000, 0x1000)],
    );
    let plan = plan_kernel_load(&image, Machine::Aarch64).expect("aarch64 image plans");
    assert_eq!(plan.entry(), 0x40_0000);
}

#[test]
fn an_image_with_no_loadable_segment_is_refused() {
    let note = Phdr {
        p_type: PT_NOTE,
        flags: PF_R,
        offset: 0,
        vaddr: 0,
        paddr: 0,
        filesz: 0,
        memsz: 0,
        align: 1,
    };
    let image = build_elf(ET_EXEC, EM_X86_64, 0, &[note]);
    assert_eq!(
        plan_kernel_load(&image, Machine::X86_64),
        Err(LoadError::NoLoadableSegments)
    );
}

#[test]
fn a_file_larger_than_memory_is_refused() {
    let mut ph = Phdr::code(0x10_0000, 0x1000);
    ph.filesz = 0x2000;
    let image = build_elf(ET_EXEC, EM_X86_64, 0x10_0000, &[ph]);
    assert_eq!(
        plan_kernel_load(&image, Machine::X86_64),
        Err(LoadError::FileLargerThanMemory)
    );
}

#[test]
fn a_zero_memory_segment_is_refused() {
    let mut ph = Phdr::code(0x10_0000, 0);
    ph.filesz = 0;
    let image = build_elf(ET_EXEC, EM_X86_64, 0x10_0000, &[ph]);
    assert_eq!(
        plan_kernel_load(&image, Machine::X86_64),
        Err(LoadError::EmptySegment)
    );
}

#[test]
fn a_file_range_past_the_image_is_refused() {
    // A segment whose file bytes claim to start past the end of the image.
    let mut ph = Phdr::code(0x10_0000, 0x1000);
    ph.offset = 0x100_0000; // far past the image
    ph.filesz = 0x10;
    let image = build_elf(ET_EXEC, EM_X86_64, 0x10_0000, &[ph]);
    // Truncate the padding the builder added so the range is genuinely out.
    let short = &image[..image.len().min(0x2000)];
    assert_eq!(
        plan_kernel_load(short, Machine::X86_64),
        Err(LoadError::FileRangeOutOfBounds)
    );
}

#[test]
fn a_non_power_of_two_alignment_is_refused() {
    let mut ph = Phdr::code(0x10_0000, 0x1000);
    ph.align = 3;
    let image = build_elf(ET_EXEC, EM_X86_64, 0x10_0000, &[ph]);
    assert_eq!(
        plan_kernel_load(&image, Machine::X86_64),
        Err(LoadError::MisalignedSegment)
    );
}

#[test]
fn a_destination_off_its_alignment_residue_is_refused() {
    // align 0x1000, offset 0, paddr not page-congruent with offset.
    let mut ph = Phdr::code(0x10_0800, 0x1000);
    ph.offset = 0;
    ph.filesz = 0x1000;
    ph.align = 0x1000;
    let image = build_elf(ET_EXEC, EM_X86_64, 0x10_0800, &[ph]);
    assert_eq!(
        plan_kernel_load(&image, Machine::X86_64),
        Err(LoadError::MisalignedSegment)
    );
}

#[test]
fn a_page_aligned_congruent_segment_is_accepted() {
    let mut ph = Phdr::code(0x10_0000, 0x1000);
    ph.offset = 0x1000;
    ph.filesz = 0x1000;
    ph.align = 0x1000;
    let image = build_elf(ET_EXEC, EM_X86_64, 0x10_0000, &[ph]);
    let plan = plan_kernel_load(&image, Machine::X86_64).expect("congruent aligned segment plans");
    assert_eq!(plan.segments()[0].phys_dest, 0x10_0000);
}

#[test]
fn a_writable_executable_segment_is_refused() {
    let mut ph = Phdr::code(0x10_0000, 0x1000);
    ph.flags = PF_R | PF_W | PF_X;
    let image = build_elf(ET_EXEC, EM_X86_64, 0x10_0000, &[ph]);
    assert_eq!(
        plan_kernel_load(&image, Machine::X86_64),
        Err(LoadError::WritableAndExecutable)
    );
}

#[test]
fn overlapping_destinations_are_refused() {
    let a = Phdr::code(0x10_0000, 0x2000);
    let b = Phdr::code(0x10_1000, 0x2000); // overlaps a's [0x100000,0x102000)
    let image = build_elf(ET_EXEC, EM_X86_64, 0x10_0000, &[a, b]);
    assert_eq!(
        plan_kernel_load(&image, Machine::X86_64),
        Err(LoadError::SegmentOverlap)
    );
}

#[test]
fn abutting_segments_do_not_count_as_overlap() {
    let a = Phdr::code(0x10_0000, 0x1000);
    let b = Phdr::code(0x10_1000, 0x1000); // starts exactly where a ends
    let image = build_elf(ET_EXEC, EM_X86_64, 0x10_0000, &[a, b]);
    let plan = plan_kernel_load(&image, Machine::X86_64).expect("abutting segments plan");
    assert_eq!(plan.segments().len(), 2);
}

#[test]
fn a_physical_range_overflow_is_refused() {
    let mut ph = Phdr::code(u64::MAX - 0x100, 0x1000);
    ph.filesz = 0; // avoid the file-range check firing first
    ph.memsz = 0x1000;
    let image = build_elf(ET_EXEC, EM_X86_64, 0, &[ph]);
    assert_eq!(
        plan_kernel_load(&image, Machine::X86_64),
        Err(LoadError::PhysRangeOverflow)
    );
}

#[test]
fn more_than_the_segment_cap_is_refused() {
    let mut phdrs = Vec::new();
    for i in 0..=(MAX_LOAD_SEGMENTS as u64) {
        let mut ph = Phdr::code(0x10_0000 + i * 0x10, 1);
        ph.filesz = 0;
        phdrs.push(ph);
    }
    let image = build_elf(ET_EXEC, EM_X86_64, 0x10_0000, &phdrs);
    assert_eq!(
        plan_kernel_load(&image, Machine::X86_64),
        Err(LoadError::TooManySegments)
    );
}

#[test]
fn exactly_the_segment_cap_is_accepted() {
    let mut phdrs = Vec::new();
    for i in 0..(MAX_LOAD_SEGMENTS as u64) {
        let mut ph = Phdr::code(0x10_0000 + i * 0x10, 1);
        ph.filesz = 0;
        phdrs.push(ph);
    }
    let image = build_elf(ET_EXEC, EM_X86_64, 0x10_0000, &phdrs);
    let plan = plan_kernel_load(&image, Machine::X86_64).expect("cap-many segments plan");
    assert_eq!(plan.segments().len(), MAX_LOAD_SEGMENTS);
}

#[test]
fn a_malformed_elf_surfaces_the_decoder_error() {
    // Not an ELF at all: the shared decoder's error is wrapped, not hidden.
    let junk = [0u8; 128];
    match plan_kernel_load(&junk, Machine::X86_64) {
        Err(LoadError::Elf(_)) => {}
        other => panic!("expected a wrapped ELF error, got {other:?}"),
    }
}

#[test]
fn segment_flags_decode_each_permission_bit() {
    assert_eq!(
        SegmentFlags::from_p_flags(PF_R),
        SegmentFlags {
            readable: true,
            writable: false,
            executable: false
        }
    );
    assert_eq!(
        SegmentFlags::from_p_flags(PF_R | PF_X),
        SegmentFlags {
            readable: true,
            writable: false,
            executable: true
        }
    );
    assert!(SegmentFlags::from_p_flags(PF_W | PF_X).is_write_execute());
    assert!(!SegmentFlags::from_p_flags(PF_R | PF_W).is_write_execute());
}

#[test]
fn segment_phys_end_saturates_to_none_on_overflow() {
    let seg = LoadSegment {
        file_offset: 0,
        file_size: 0,
        phys_dest: u64::MAX,
        mem_size: 1,
        flags: SegmentFlags::from_p_flags(PF_R),
    };
    assert_eq!(seg.phys_end(), None);
}
