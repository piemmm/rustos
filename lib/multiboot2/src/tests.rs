//! Host tests for the shared Multiboot2 wire layout.
//!
//! Two families:
//!
//! * **Round-trip** — the [`InfoBuilder`] assembles a structure and the
//!   [`BootInfo`] parser reads it back, proving the producer and consumer
//!   agree on every tag's layout to the byte (they are the two halves of
//!   the boot handoff and must never drift).
//! * **Refusal** — the parser rejects every malformed stream and the
//!   builder fails closed on an undersized buffer or an unrepresentable
//!   command line.

extern crate std;

use std::vec::Vec;

use super::*;

/// 8-byte-aligned scratch buffer (the Multiboot2 spec requires the
/// information structure to live on an 8-byte boundary, and the parser
/// enforces it).
#[repr(C, align(8))]
struct Aligned<const N: usize>([u8; N]);

impl<const N: usize> Aligned<N> {
    fn new() -> Self {
        Aligned([0u8; N])
    }
}

fn put_u32_test(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64_test(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

// --- Round-trip: builder -> parser -----------------------------------

#[test]
fn round_trip_all_tags() {
    let entries = [
        Mb2MemoryEntry {
            base: 0,
            length: 0x9_fc00,
            kind: Mb2MemoryKind::Available,
        },
        Mb2MemoryEntry {
            base: 0x10_0000,
            length: 0x7f00_0000,
            kind: Mb2MemoryKind::Available,
        },
        Mb2MemoryEntry {
            base: 0xfee0_0000,
            length: 0x1000,
            kind: Mb2MemoryKind::Reserved,
        },
        Mb2MemoryEntry {
            base: 0x7fe0_0000,
            length: 0x10_0000,
            kind: Mb2MemoryKind::AcpiReclaimable,
        },
    ];
    let rsdp = [0xABu8; 36];
    let fb = FramebufferInfo {
        addr: 0xfd00_0000,
        pitch: 0x1000,
        width: 1024,
        height: 768,
        bpp: 32,
        fb_type: FRAMEBUFFER_TYPE_RGB,
        red_field_position: 16,
        red_mask_size: 8,
        green_field_position: 8,
        green_mask_size: 8,
        blue_field_position: 0,
        blue_mask_size: 8,
    };

    let mut buf = Aligned::<512>::new();
    let bytes = {
        let mut b = InfoBuilder::new(&mut buf.0).expect("buffer large enough");
        b.basic_memory(640, 0x0007_f000).expect("basic memory");
        b.command_line("root=disk0 quiet").expect("cmdline");
        b.memory_map(&entries).expect("memory map");
        b.rsdp(true, &rsdp).expect("rsdp");
        b.framebuffer(&fb).expect("framebuffer");
        b.finish()
    };

    // The produced bytes live in the aligned buffer, so re-parse them
    // through the original 8-aligned storage.
    let info = BootInfo::parse(bytes).expect("built structure parses");

    // Basic memory.
    let mut saw_basic = false;
    for tag in info.tags() {
        if let Tag::BasicMemory {
            lower_kib,
            upper_kib,
        } = tag
        {
            assert_eq!(lower_kib, 640);
            assert_eq!(upper_kib, 0x0007_f000);
            saw_basic = true;
        }
    }
    assert!(saw_basic, "basic-memory tag round-trips");

    // Command line.
    assert_eq!(info.command_line(), Some("root=disk0 quiet"));

    // Memory map.
    let mmap = info.memory_map().expect("memory map present");
    let got: Vec<_> = mmap.entries().collect();
    assert_eq!(got.as_slice(), &entries);

    // RSDP (v2 preferred, and it is the only one here).
    assert_eq!(info.rsdp(), Some(&rsdp[..]));

    // Framebuffer.
    assert_eq!(info.framebuffer(), Some(fb));
}

#[test]
fn round_trip_empty_stream() {
    let mut buf = Aligned::<32>::new();
    let bytes = InfoBuilder::new(&mut buf.0).expect("min buffer").finish();
    // total_size = 8 (header) + 8 (end tag).
    assert_eq!(bytes.len(), 16);
    let info = BootInfo::parse(bytes).expect("empty stream parses");
    assert!(info.tags().next().is_none());
    assert!(info.memory_map().is_none());
    assert!(info.command_line().is_none());
    assert!(info.framebuffer().is_none());
    assert!(info.rsdp().is_none());
}

#[test]
fn round_trip_rsdp_v1() {
    let rsdp = [0x11u8; 20];
    let mut buf = Aligned::<64>::new();
    let bytes = {
        let mut b = InfoBuilder::new(&mut buf.0).expect("buffer");
        b.rsdp(false, &rsdp).expect("rsdp v1");
        b.finish()
    };
    let info = BootInfo::parse(bytes).expect("parses");
    // With only a v1 RSDP, `rsdp()` returns it.
    assert_eq!(info.rsdp(), Some(&rsdp[..]));
    // And the tag decodes as the v1 variant.
    assert!(matches!(
        info.tags().next(),
        Some(Tag::Rsdp { v2: false, .. })
    ));
}

#[test]
fn round_trip_non_rgb_framebuffer_has_zero_color() {
    // An EGA-text framebuffer (type 2) carries no meaningful RGB colour
    // info; the builder always writes the six colour bytes, but the parser
    // only surfaces them for the RGB type, so they read back as zero.
    let fb = FramebufferInfo {
        addr: 0xb8000,
        pitch: 160,
        width: 80,
        height: 25,
        bpp: 16,
        fb_type: 2,
        red_field_position: 9,
        red_mask_size: 9,
        green_field_position: 9,
        green_mask_size: 9,
        blue_field_position: 9,
        blue_mask_size: 9,
    };
    let mut buf = Aligned::<64>::new();
    let bytes = {
        let mut b = InfoBuilder::new(&mut buf.0).expect("buffer");
        b.framebuffer(&fb).expect("framebuffer");
        b.finish()
    };
    let info = BootInfo::parse(bytes).expect("parses");
    let got = info.framebuffer().expect("framebuffer present");
    assert_eq!(got.fb_type, 2);
    assert_eq!(got.width, 80);
    assert_eq!(got.red_field_position, 0, "non-RGB colour reads back zero");
    assert_eq!(got.blue_mask_size, 0);
}

// --- Builder refusals ------------------------------------------------

#[test]
fn builder_rejects_tiny_buffer() {
    let mut buf = [0u8; 8]; // less than header + end tag (16)
    assert_eq!(
        InfoBuilder::new(&mut buf).err(),
        Some(BuildError::BufferTooSmall)
    );
}

#[test]
fn builder_fails_closed_when_tag_would_not_fit() {
    // Room for the header + end tag but not a large RSDP payload.
    let mut buf = Aligned::<24>::new();
    let mut b = InfoBuilder::new(&mut buf.0).expect("min buffer");
    assert_eq!(
        b.rsdp(true, &[0u8; 36]).err(),
        Some(BuildError::BufferTooSmall)
    );
}

#[test]
fn builder_rejects_interior_nul_command_line() {
    let mut buf = Aligned::<64>::new();
    let mut b = InfoBuilder::new(&mut buf.0).expect("buffer");
    assert_eq!(
        b.command_line("bad\0cmd").err(),
        Some(BuildError::InvalidString)
    );
}

#[test]
fn builder_partial_stream_still_terminates_and_parses() {
    // Appending, hitting a refusal, then finishing must still yield a
    // valid, parseable stream containing what did fit (fail-closed: the
    // failed tag is simply absent).
    let mut buf = Aligned::<40>::new();
    let bytes = {
        let mut b = InfoBuilder::new(&mut buf.0).expect("buffer");
        b.basic_memory(1, 2).expect("basic memory fits");
        // This RSDP will not fit; the append is refused but the builder
        // state is unchanged.
        assert_eq!(
            b.rsdp(true, &[0u8; 36]).err(),
            Some(BuildError::BufferTooSmall)
        );
        b.finish()
    };
    let info = BootInfo::parse(bytes).expect("parses");
    assert!(matches!(info.tags().next(), Some(Tag::BasicMemory { .. })));
    assert!(info.rsdp().is_none());
}

// --- Parser refusals and decoding (migrated from the kernel module) --

#[test]
fn parse_rejects_misaligned() {
    let mut buf = Aligned::<24>::new();
    put_u32_test(&mut buf.0, 0, 16); // total_size
    put_u32_test(&mut buf.0, 8, TAG_END);
    put_u32_test(&mut buf.0, 12, 8);
    let aligned_ok = BootInfo::parse(&buf.0[..16]).unwrap();
    assert!(aligned_ok.tags().next().is_none());
    // 1-byte offset breaks alignment.
    let misaligned = &buf.0[1..17];
    assert_eq!(
        BootInfo::parse(misaligned).err(),
        Some(ParseError::Misaligned)
    );
}

#[test]
fn parse_rejects_truncated_header() {
    let buf = Aligned::<4>::new();
    assert_eq!(
        BootInfo::parse(&buf.0).err(),
        Some(ParseError::HeaderTruncated)
    );
}

#[test]
fn parse_rejects_inconsistent_total_size() {
    let mut buf = Aligned::<16>::new();
    put_u32_test(&mut buf.0, 0, 9999); // larger than slice
    assert_eq!(
        BootInfo::parse(&buf.0).err(),
        Some(ParseError::HeaderInconsistent)
    );
    put_u32_test(&mut buf.0, 0, 4); // smaller than header
    assert_eq!(
        BootInfo::parse(&buf.0).err(),
        Some(ParseError::HeaderInconsistent)
    );
}

#[test]
fn parse_requires_end_tag() {
    let mut buf = Aligned::<16>::new();
    put_u32_test(&mut buf.0, 0, 16); // total_size
    put_u32_test(&mut buf.0, 8, 99); // non-end tag, size 8, no terminator
    put_u32_test(&mut buf.0, 12, 8);
    assert_eq!(
        BootInfo::parse(&buf.0).err(),
        Some(ParseError::TagTruncated)
    );
}

#[test]
fn memory_map_rejects_tiny_entry_size() {
    let mut buf = Aligned::<32>::new();
    put_u32_test(&mut buf.0, 0, 32);
    put_u32_test(&mut buf.0, 8, TAG_MMAP);
    put_u32_test(&mut buf.0, 12, 16); // tag size — payload only 8 bytes
    put_u32_test(&mut buf.0, 16, 8); // entry_size below the 24-byte minimum
    put_u32_test(&mut buf.0, 20, 0);
    put_u32_test(&mut buf.0, 24, TAG_END);
    put_u32_test(&mut buf.0, 28, 8);
    let info = BootInfo::parse(&buf.0).unwrap();
    // A malformed mmap tag degrades to Tag::Other rather than tearing
    // down the whole stream (forward-compat behaviour).
    assert!(info.memory_map().is_none());
    assert!(matches!(info.tags().next(), Some(Tag::Other(t)) if t == TAG_MMAP));
}

#[test]
fn efi_memory_map_decodes_entries() {
    // header(8) + tag header(8) + tag-local header(16) + 1*40 + end(8) = 80
    let mut buf = Aligned::<80>::new();
    put_u32_test(&mut buf.0, 0, 80);
    put_u32_test(&mut buf.0, 8, TAG_EFI_MMAP);
    put_u32_test(&mut buf.0, 12, 8 + 16 + 40); // tag size = 64
    put_u32_test(&mut buf.0, 16, 40); // descriptor_size
    put_u32_test(&mut buf.0, 20, 1); // descriptor_version
                                     // Descriptor at offset 32: kind=7 (EfiConventionalMemory),
                                     // physical=0x100000, virtual=0, pages=0x10, attr=0xF.
    put_u32_test(&mut buf.0, 32, 7);
    put_u64_test(&mut buf.0, 40, 0x10_0000);
    put_u64_test(&mut buf.0, 48, 0);
    put_u64_test(&mut buf.0, 56, 0x10);
    put_u64_test(&mut buf.0, 64, 0xF);
    put_u32_test(&mut buf.0, 72, TAG_END);
    put_u32_test(&mut buf.0, 76, 8);

    let info = BootInfo::parse(&buf.0).unwrap();
    let efi = info.efi_memory_map().expect("efi tag");
    let entries: Vec<_> = efi.entries().collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, 7);
    assert_eq!(entries[0].physical_start, 0x10_0000);
    assert_eq!(entries[0].length_bytes(), 0x10 * 4096);
    assert!(entries[0].is_usable_after_exit_boot_services());
}

#[test]
fn unknown_tag_becomes_other() {
    let mut buf = Aligned::<24>::new();
    put_u32_test(&mut buf.0, 0, 24);
    put_u32_test(&mut buf.0, 8, 999); // forward-compat unknown
    put_u32_test(&mut buf.0, 12, 8);
    put_u32_test(&mut buf.0, 16, TAG_END);
    put_u32_test(&mut buf.0, 20, 8);
    let info = BootInfo::parse(&buf.0).unwrap();
    assert!(matches!(info.tags().next(), Some(Tag::Other(999))));
}

#[test]
fn command_line_without_nul_degrades_to_other() {
    // A type-1 tag whose payload has no NUL terminator is malformed and
    // degrades to `Tag::Other` rather than exposing an unterminated string.
    // cmdline tag (size 12, padded to 16) at offset 8 occupies 8..24, then
    // the end tag at offset 24; total_size = 32.
    let mut buf = Aligned::<32>::new();
    put_u32_test(&mut buf.0, 0, 32);
    put_u32_test(&mut buf.0, 8, TAG_CMDLINE);
    put_u32_test(&mut buf.0, 12, 12); // header(8) + 4 payload bytes
    buf.0[16] = b'a';
    buf.0[17] = b'b';
    buf.0[18] = b'c';
    buf.0[19] = b'd'; // no NUL
    put_u32_test(&mut buf.0, 24, TAG_END);
    put_u32_test(&mut buf.0, 28, 8);
    let info = BootInfo::parse(&buf.0).unwrap();
    assert!(info.command_line().is_none());
    assert!(matches!(info.tags().next(), Some(Tag::Other(t)) if t == TAG_CMDLINE));
}

#[test]
fn memory_kind_raw_round_trips_canonical_codes() {
    for k in [
        Mb2MemoryKind::Available,
        Mb2MemoryKind::Reserved,
        Mb2MemoryKind::AcpiReclaimable,
        Mb2MemoryKind::AcpiNvs,
        Mb2MemoryKind::Defective,
    ] {
        assert_eq!(Mb2MemoryKind::from_raw(k.to_raw()), k);
    }
}
