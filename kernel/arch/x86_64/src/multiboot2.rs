//! Zero-copy `no_alloc` parser for the Multiboot2 information structure.
//!
//! The Multiboot2 specification (rev 2.0) defines a tag-stream
//! handed from the boot-loader to the kernel: a 16-byte header
//! (`total_size`, `reserved`) followed by a sequence of 8-byte-aligned
//! tags. Each tag begins with a 4-byte `type`, a 4-byte `size`
//! (header inclusive), and a tag-specific payload. The stream is
//! terminated by a tag of type `0` and size `8`.
//!
//! This module exposes a borrow-only view over a Multiboot2 buffer
//! provided by the boot-loader. **Nothing here allocates**; the parser
//! validates that every tag is fully contained in the input slice and
//! refuses (returns `Err`) on truncation, mis-alignment, or overflow.
//! Higher-level consumers (`bootmemory`, `acpi`) translate the typed
//! tag stream into `BootMemoryMap` regions and ACPI RSDP pointers.
//!
//! # Tags recognised by this parser
//!
//! | Type | Name              | Used for                                |
//! |-----:|-------------------|-----------------------------------------|
//! |   0  | End               | Stream terminator.                      |
//! |   4  | Basic memory      | Legacy `mem_lower`/`mem_upper` (info).  |
//! |   6  | Memory map        | BIOS-derived physical memory map.       |
//! |  14  | ACPI 1.0 RSDP     | 20-byte RSDP descriptor.                |
//! |  15  | ACPI 2.0 RSDP     | 36-byte XSDT-capable RSDP descriptor.   |
//! |  17  | EFI memory map    | Raw EFI memory descriptors.             |
//!
//! Other tag types are *not* an error; they are silently skipped so the
//! parser is forward-compatible with future Multiboot2 revisions.
//!
//! # References
//!
//! * Multiboot2 specification, rev 2.0 ("Boot information format").
//! * UEFI 2.10, §7.2 ("Memory Allocation Services") — for the EFI
//!   descriptor layout carried by tag 17.

#![allow(clippy::module_name_repetitions)]

use core::mem::size_of;

/// Tag-stream parsing errors. All variants are closed-fail conditions
/// (validate every input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Input buffer is not 8-byte aligned (the spec mandates this).
    Misaligned,
    /// Input buffer is shorter than the 8-byte header.
    HeaderTruncated,
    /// `total_size` in the header is less than `8` or exceeds the slice.
    HeaderInconsistent,
    /// A tag's `size` field is less than `8` or runs past the buffer,
    /// or the stream ended without an explicit end tag.
    TagTruncated,
}

/// Multiboot2 information-structure header.
///
/// 16 bytes: `total_size: u32`, `reserved: u32`, then tags.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Header {
    total_size: u32,
    _reserved: u32,
}

/// Multiboot2 tag header (every tag starts with this 8-byte prefix).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct TagHeader {
    tag_type: u32,
    size: u32,
}

const TAG_END: u32 = 0;
const TAG_BASIC_MEMORY: u32 = 4;
const TAG_MMAP: u32 = 6;
const TAG_RSDP_V1: u32 = 14;
const TAG_RSDP_V2: u32 = 15;
const TAG_EFI_MMAP: u32 = 17;

/// Parsed, borrow-only view over a Multiboot2 information structure.
///
/// Construct via [`BootInfo::parse`]. The view holds a reference to the
/// caller-supplied buffer; the caller is responsible for ensuring the
/// buffer outlives the [`BootInfo`].
#[derive(Debug, Clone, Copy)]
pub struct BootInfo<'a> {
    /// The portion of the input slice that contains tags (i.e. after
    /// the 8-byte header and up to `total_size`).
    tag_bytes: &'a [u8],
}

impl<'a> BootInfo<'a> {
    /// Parse and validate a Multiboot2 information buffer.
    ///
    /// The buffer must be 8-byte aligned (the boot-loader places it on
    /// such an address by spec) and must be at least `total_size` bytes
    /// long. Returns a borrow-only view; no allocation occurs.
    ///
    /// # Errors
    ///
    /// Returns a [`ParseError`] for any structural defect; see the enum
    /// for the closed-fail conditions.
    pub fn parse(buf: &'a [u8]) -> Result<Self, ParseError> {
        if (buf.as_ptr() as usize) % 8 != 0 {
            return Err(ParseError::Misaligned);
        }
        if buf.len() < size_of::<Header>() {
            return Err(ParseError::HeaderTruncated);
        }
        // Read total_size out of the first 4 bytes. We do not transmute;
        // the slice is checked for length above and alignment is at
        // least 4 because we already required 8.
        let total_size = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if total_size < size_of::<Header>() || total_size > buf.len() {
            return Err(ParseError::HeaderInconsistent);
        }
        let tag_bytes = &buf[size_of::<Header>()..total_size];
        // Walk the stream once up-front to catch any structural issue
        // (truncated tags, missing end tag). This lets every
        // downstream iterator be infallible.
        Self::validate(tag_bytes)?;
        Ok(Self { tag_bytes })
    }

    fn validate(mut rest: &[u8]) -> Result<(), ParseError> {
        loop {
            if rest.len() < size_of::<TagHeader>() {
                return Err(ParseError::TagTruncated);
            }
            let tag_type = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
            let size = u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
            if size < size_of::<TagHeader>() || size > rest.len() {
                return Err(ParseError::TagTruncated);
            }
            if tag_type == TAG_END {
                if size != size_of::<TagHeader>() {
                    return Err(ParseError::TagTruncated);
                }
                return Ok(());
            }
            // Tags are 8-byte aligned in the stream.
            let advance = (size + 7) & !7;
            if advance > rest.len() {
                return Err(ParseError::TagTruncated);
            }
            rest = &rest[advance..];
        }
    }

    /// Iterator over every recognised tag.
    #[must_use]
    pub fn tags(&self) -> TagIter<'a> {
        TagIter {
            rest: self.tag_bytes,
        }
    }

    /// Convenience: the first memory-map tag (Multiboot2 type 6), if any.
    #[must_use]
    pub fn memory_map(&self) -> Option<MemoryMap<'a>> {
        for tag in self.tags() {
            if let Tag::MemoryMap(m) = tag {
                return Some(m);
            }
        }
        None
    }

    /// Convenience: the first EFI memory-map tag (Multiboot2 type 17).
    #[must_use]
    pub fn efi_memory_map(&self) -> Option<EfiMemoryMap<'a>> {
        for tag in self.tags() {
            if let Tag::EfiMemoryMap(m) = tag {
                return Some(m);
            }
        }
        None
    }

    /// Convenience: the ACPI 2.0 RSDP if present, else the ACPI 1.0 one.
    #[must_use]
    pub fn rsdp(&self) -> Option<&'a [u8]> {
        let mut v1 = None;
        for tag in self.tags() {
            match tag {
                Tag::Rsdp { v2: true, bytes } => return Some(bytes),
                Tag::Rsdp { v2: false, bytes } => v1 = Some(bytes),
                _ => {}
            }
        }
        v1
    }
}

/// One recognised tag from the Multiboot2 stream.
#[derive(Debug, Clone, Copy)]
pub enum Tag<'a> {
    /// Tag types we accept but expose nothing for (forward compat).
    Other(u32),
    /// Multiboot2 type 4 — `mem_lower` and `mem_upper`.
    BasicMemory {
        /// Lower memory in KiB (640 KiB conventional region).
        lower_kib: u32,
        /// Upper memory in KiB starting at 1 MiB.
        upper_kib: u32,
    },
    /// Multiboot2 type 6 — BIOS-derived memory map.
    MemoryMap(MemoryMap<'a>),
    /// Multiboot2 type 17 — EFI memory map (raw EFI descriptors).
    EfiMemoryMap(EfiMemoryMap<'a>),
    /// Multiboot2 type 14 (`v2 == false`) or 15 (`v2 == true`).
    Rsdp {
        /// `true` for tag 15 (ACPI 2.0+ XSDT-capable RSDP), `false`
        /// for tag 14 (ACPI 1.0 RSDP).
        v2: bool,
        /// Raw RSDP descriptor bytes (20 bytes for v1, 36 for v2).
        bytes: &'a [u8],
    },
}

/// Iterator over `Tag` values in a validated tag stream.
#[derive(Debug, Clone)]
pub struct TagIter<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for TagIter<'a> {
    type Item = Tag<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        // `validate` ran in `BootInfo::parse`, so every read here is in
        // bounds. We re-check defensively anyway (cheap, and the charter forbids "trust me" comments).
        if self.rest.len() < size_of::<TagHeader>() {
            return None;
        }
        let tag_type = u32::from_le_bytes([self.rest[0], self.rest[1], self.rest[2], self.rest[3]]);
        let size =
            u32::from_le_bytes([self.rest[4], self.rest[5], self.rest[6], self.rest[7]]) as usize;
        if tag_type == TAG_END {
            self.rest = &[];
            return None;
        }
        if size < size_of::<TagHeader>() || size > self.rest.len() {
            self.rest = &[];
            return None;
        }
        let payload = &self.rest[size_of::<TagHeader>()..size];
        let advance = (size + 7) & !7;
        self.rest = if advance <= self.rest.len() {
            &self.rest[advance..]
        } else {
            &[]
        };
        Some(decode(tag_type, payload))
    }
}

fn decode(tag_type: u32, payload: &[u8]) -> Tag<'_> {
    match tag_type {
        TAG_BASIC_MEMORY => {
            if payload.len() < 8 {
                return Tag::Other(tag_type);
            }
            let lo = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
            let hi = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
            Tag::BasicMemory {
                lower_kib: lo,
                upper_kib: hi,
            }
        }
        TAG_MMAP => MemoryMap::decode(payload).map_or(Tag::Other(tag_type), Tag::MemoryMap),
        TAG_EFI_MMAP => {
            EfiMemoryMap::decode(payload).map_or(Tag::Other(tag_type), Tag::EfiMemoryMap)
        }
        TAG_RSDP_V1 => Tag::Rsdp {
            v2: false,
            bytes: payload,
        },
        TAG_RSDP_V2 => Tag::Rsdp {
            v2: true,
            bytes: payload,
        },
        _ => Tag::Other(tag_type),
    }
}

// --- Multiboot2 memory map (type 6) ----------------------------------

/// Entry type for the BIOS-derived memory map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mb2MemoryKind {
    /// `type == 1` — usable RAM.
    Available,
    /// `type == 3` — ACPI reclaimable.
    AcpiReclaimable,
    /// `type == 4` — ACPI NVS.
    AcpiNvs,
    /// `type == 5` — bad memory.
    Defective,
    /// Anything else — reserved.
    Reserved,
}

impl Mb2MemoryKind {
    fn from_raw(v: u32) -> Self {
        match v {
            1 => Self::Available,
            3 => Self::AcpiReclaimable,
            4 => Self::AcpiNvs,
            5 => Self::Defective,
            _ => Self::Reserved,
        }
    }
}

/// One entry in the BIOS-derived memory map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mb2MemoryEntry {
    /// Physical base of the region.
    pub base: u64,
    /// Length in bytes.
    pub length: u64,
    /// Kind decoded from the raw `type` field.
    pub kind: Mb2MemoryKind,
}

/// Memory-map tag view: 8-byte header (`entry_size`, `entry_version`)
/// followed by `entry_size`-strided entries.
#[derive(Debug, Clone, Copy)]
pub struct MemoryMap<'a> {
    entry_size: usize,
    entries_bytes: &'a [u8],
}

impl<'a> MemoryMap<'a> {
    fn decode(payload: &'a [u8]) -> Option<Self> {
        if payload.len() < 8 {
            return None;
        }
        let entry_size =
            u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
        // entry_version at offset 4..8; we accept any version, the
        // 24-byte layout (base, length, type, reserved) is stable.
        if entry_size < 24 {
            return None;
        }
        let body = &payload[8..];
        if body.len() % entry_size != 0 {
            return None;
        }
        Some(Self {
            entry_size,
            entries_bytes: body,
        })
    }

    /// Iterate the typed entries.
    #[must_use]
    pub fn entries(&self) -> Mb2MemoryEntryIter<'a> {
        Mb2MemoryEntryIter {
            entry_size: self.entry_size,
            rest: self.entries_bytes,
        }
    }
}

/// Iterator yielded by [`MemoryMap::entries`].
#[derive(Debug, Clone)]
pub struct Mb2MemoryEntryIter<'a> {
    entry_size: usize,
    rest: &'a [u8],
}

impl Iterator for Mb2MemoryEntryIter<'_> {
    type Item = Mb2MemoryEntry;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.len() < self.entry_size {
            return None;
        }
        let entry = &self.rest[..self.entry_size];
        self.rest = &self.rest[self.entry_size..];
        let base = u64::from_le_bytes([
            entry[0], entry[1], entry[2], entry[3], entry[4], entry[5], entry[6], entry[7],
        ]);
        let length = u64::from_le_bytes([
            entry[8], entry[9], entry[10], entry[11], entry[12], entry[13], entry[14], entry[15],
        ]);
        let raw_kind = u32::from_le_bytes([entry[16], entry[17], entry[18], entry[19]]);
        Some(Mb2MemoryEntry {
            base,
            length,
            kind: Mb2MemoryKind::from_raw(raw_kind),
        })
    }
}

// --- EFI memory map (type 17) ---------------------------------------

/// EFI memory descriptor type as defined by UEFI 2.10 §7.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EfiMemoryDescriptor {
    /// `EfiMemoryType` — see UEFI 2.10 §7.2 Table 7-9. Raw u32.
    pub kind: u32,
    /// Physical start address (page-aligned per spec).
    pub physical_start: u64,
    /// Virtual start address (zero before `SetVirtualAddressMap`).
    pub virtual_start: u64,
    /// Number of 4 KiB pages.
    pub number_of_pages: u64,
    /// Attribute bits (cacheability, runtime, …).
    pub attribute: u64,
}

impl EfiMemoryDescriptor {
    /// Length in bytes (number of pages * 4096). Saturates at `u64::MAX`.
    #[must_use]
    pub fn length_bytes(&self) -> u64 {
        self.number_of_pages.saturating_mul(4096)
    }

    /// `true` if this descriptor describes free, post-boot-services RAM
    /// per UEFI 2.10 Table 7-9 (`EfiConventionalMemory == 7`,
    /// `EfiBootServicesCode == 3`, `EfiBootServicesData == 4`, and
    /// `EfiLoaderCode == 1` / `EfiLoaderData == 2` which the loader
    /// itself owned and is releasing). All other types are treated as
    /// reserved.
    #[must_use]
    pub fn is_usable_after_exit_boot_services(&self) -> bool {
        matches!(self.kind, 1 | 2 | 3 | 4 | 7)
    }
}

/// EFI memory-map tag view: 12-byte header
/// (`descriptor_size`, `descriptor_version`, `_reserved`) followed by
/// `descriptor_size`-strided raw `EFI_MEMORY_DESCRIPTOR` records.
#[derive(Debug, Clone, Copy)]
pub struct EfiMemoryMap<'a> {
    descriptor_size: usize,
    entries_bytes: &'a [u8],
}

impl<'a> EfiMemoryMap<'a> {
    fn decode(payload: &'a [u8]) -> Option<Self> {
        if payload.len() < 16 {
            return None;
        }
        let descriptor_size =
            u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
        // The UEFI EFI_MEMORY_DESCRIPTOR is exactly 40 bytes today but
        // the spec permits the firmware to extend it; treat anything
        // smaller as malformed.
        if descriptor_size < 40 {
            return None;
        }
        let body = &payload[16..];
        if body.len() % descriptor_size != 0 {
            return None;
        }
        Some(Self {
            descriptor_size,
            entries_bytes: body,
        })
    }

    /// Iterate the typed descriptors.
    #[must_use]
    pub fn entries(&self) -> EfiMemoryEntryIter<'a> {
        EfiMemoryEntryIter {
            descriptor_size: self.descriptor_size,
            rest: self.entries_bytes,
        }
    }
}

/// Iterator yielded by [`EfiMemoryMap::entries`].
#[derive(Debug, Clone)]
pub struct EfiMemoryEntryIter<'a> {
    descriptor_size: usize,
    rest: &'a [u8],
}

impl Iterator for EfiMemoryEntryIter<'_> {
    type Item = EfiMemoryDescriptor;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.len() < self.descriptor_size {
            return None;
        }
        let entry = &self.rest[..self.descriptor_size];
        self.rest = &self.rest[self.descriptor_size..];
        let kind = u32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]);
        // 4-byte padding sits between `kind` and `physical_start`.
        let physical_start = u64::from_le_bytes([
            entry[8], entry[9], entry[10], entry[11], entry[12], entry[13], entry[14], entry[15],
        ]);
        let virtual_start = u64::from_le_bytes([
            entry[16], entry[17], entry[18], entry[19], entry[20], entry[21], entry[22], entry[23],
        ]);
        let number_of_pages = u64::from_le_bytes([
            entry[24], entry[25], entry[26], entry[27], entry[28], entry[29], entry[30], entry[31],
        ]);
        let attribute = u64::from_le_bytes([
            entry[32], entry[33], entry[34], entry[35], entry[36], entry[37], entry[38], entry[39],
        ]);
        Some(EfiMemoryDescriptor {
            kind,
            physical_start,
            virtual_start,
            number_of_pages,
            attribute,
        })
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    // Buffer aligned to 8 bytes on the stack via a wrapping
    // `#[repr(C, align(8))]` struct (the Multiboot2 spec requires the
    // information structure itself to live on an 8-byte boundary).
    #[repr(C, align(8))]
    struct Aligned<const N: usize>([u8; N]);

    fn put_u32(buf: &mut [u8], off: usize, v: u32) {
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u64(buf: &mut [u8], off: usize, v: u64) {
        buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    #[test]
    fn parse_rejects_misaligned() {
        let mut buf = Aligned::<24>([0u8; 24]);
        // Build a minimal-but-valid buffer first, then take an unaligned
        // sub-slice to exercise the alignment check.
        put_u32(&mut buf.0, 0, 16); // total_size
                                    // end tag at offset 8
        put_u32(&mut buf.0, 8, TAG_END);
        put_u32(&mut buf.0, 12, 8);
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
        let buf = Aligned::<4>([0u8; 4]);
        assert_eq!(
            BootInfo::parse(&buf.0).err(),
            Some(ParseError::HeaderTruncated)
        );
    }

    #[test]
    fn parse_rejects_inconsistent_total_size() {
        let mut buf = Aligned::<16>([0u8; 16]);
        put_u32(&mut buf.0, 0, 9999); // larger than slice
        assert_eq!(
            BootInfo::parse(&buf.0).err(),
            Some(ParseError::HeaderInconsistent)
        );
        put_u32(&mut buf.0, 0, 4); // smaller than header
        assert_eq!(
            BootInfo::parse(&buf.0).err(),
            Some(ParseError::HeaderInconsistent)
        );
    }

    #[test]
    fn parse_requires_end_tag() {
        let mut buf = Aligned::<16>([0u8; 16]);
        put_u32(&mut buf.0, 0, 16); // total_size
                                    // A "fake" non-end tag of size 8 followed by no terminator.
        put_u32(&mut buf.0, 8, 99);
        put_u32(&mut buf.0, 12, 8);
        // Walk hits end-of-buffer before seeing TAG_END.
        assert_eq!(
            BootInfo::parse(&buf.0).err(),
            Some(ParseError::TagTruncated)
        );
    }

    #[test]
    fn parse_accepts_minimal_stream() {
        let mut buf = Aligned::<16>([0u8; 16]);
        put_u32(&mut buf.0, 0, 16);
        put_u32(&mut buf.0, 8, TAG_END);
        put_u32(&mut buf.0, 12, 8);
        let info = BootInfo::parse(&buf.0).unwrap();
        assert!(info.tags().next().is_none());
        assert!(info.memory_map().is_none());
        assert!(info.efi_memory_map().is_none());
        assert!(info.rsdp().is_none());
    }

    #[test]
    fn memory_map_decodes_entries() {
        // total_size = 16 (header) + 32 (mmap tag) + 8 (end) = 56
        let mut buf = Aligned::<56>([0u8; 56]);
        put_u32(&mut buf.0, 0, 56);
        // mmap tag at offset 8
        put_u32(&mut buf.0, 8, TAG_MMAP);
        put_u32(&mut buf.0, 12, 8 + 8 + 24); // tag size = 40
        put_u32(&mut buf.0, 16, 24); // entry_size
        put_u32(&mut buf.0, 20, 0); // entry_version
                                    // entry: base=0x1000, length=0x2000, type=1, reserved=0
        put_u64(&mut buf.0, 24, 0x1000);
        put_u64(&mut buf.0, 32, 0x2000);
        put_u32(&mut buf.0, 40, 1);
        put_u32(&mut buf.0, 44, 0);
        // end tag at offset 48
        put_u32(&mut buf.0, 48, TAG_END);
        put_u32(&mut buf.0, 52, 8);

        let info = BootInfo::parse(&buf.0).unwrap();
        let mmap = info.memory_map().expect("mmap tag");
        let entries: Vec<_> = mmap.entries().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].base, 0x1000);
        assert_eq!(entries[0].length, 0x2000);
        assert_eq!(entries[0].kind, Mb2MemoryKind::Available);
    }

    #[test]
    fn memory_map_rejects_tiny_entry_size() {
        let mut buf = Aligned::<32>([0u8; 32]);
        put_u32(&mut buf.0, 0, 32);
        put_u32(&mut buf.0, 8, TAG_MMAP);
        put_u32(&mut buf.0, 12, 16); // tag size — payload only 8 bytes
        put_u32(&mut buf.0, 16, 8); // entry_size below the 24-byte minimum
        put_u32(&mut buf.0, 20, 0);
        put_u32(&mut buf.0, 24, TAG_END);
        put_u32(&mut buf.0, 28, 8);
        let info = BootInfo::parse(&buf.0).unwrap();
        // A malformed mmap tag degrades to Tag::Other rather than
        // tearing down the whole stream (forward-compat behaviour).
        assert!(info.memory_map().is_none());
        assert!(matches!(info.tags().next(), Some(Tag::Other(t)) if t == TAG_MMAP));
    }

    #[test]
    fn efi_memory_map_decodes_entries() {
        // header(16) + tag header(8) + tag-local header(16) + 1*40 +
        // end(8) = 88
        let mut buf = Aligned::<88>([0u8; 88]);
        put_u32(&mut buf.0, 0, 88);
        put_u32(&mut buf.0, 8, TAG_EFI_MMAP);
        put_u32(&mut buf.0, 12, 8 + 16 + 40); // tag size = 64
        put_u32(&mut buf.0, 16, 40); // descriptor_size
        put_u32(&mut buf.0, 20, 1); // descriptor_version
                                    // 8 bytes reserved already zero.
                                    // Descriptor at offset 32: kind=7 (EfiConventionalMemory),
                                    // physical=0x100000, virtual=0, pages=0x10 (=64 KiB), attr=0xF.
        put_u32(&mut buf.0, 32, 7);
        put_u64(&mut buf.0, 40, 0x10_0000);
        put_u64(&mut buf.0, 48, 0);
        put_u64(&mut buf.0, 56, 0x10);
        put_u64(&mut buf.0, 64, 0xF);
        put_u32(&mut buf.0, 72, TAG_END);
        put_u32(&mut buf.0, 76, 8);

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
    fn basic_memory_tag_decodes() {
        let mut buf = Aligned::<32>([0u8; 32]);
        put_u32(&mut buf.0, 0, 32);
        put_u32(&mut buf.0, 8, TAG_BASIC_MEMORY);
        put_u32(&mut buf.0, 12, 16);
        put_u32(&mut buf.0, 16, 640); // lower_kib
        put_u32(&mut buf.0, 20, 64512); // upper_kib (~64 MiB)
        put_u32(&mut buf.0, 24, TAG_END);
        put_u32(&mut buf.0, 28, 8);
        let info = BootInfo::parse(&buf.0).unwrap();
        match info.tags().next() {
            Some(Tag::BasicMemory {
                lower_kib,
                upper_kib,
            }) => {
                assert_eq!(lower_kib, 640);
                assert_eq!(upper_kib, 64512);
            }
            other => panic!("unexpected tag: {other:?}"),
        }
    }

    #[test]
    fn unknown_tag_becomes_other() {
        let mut buf = Aligned::<24>([0u8; 24]);
        put_u32(&mut buf.0, 0, 24);
        put_u32(&mut buf.0, 8, 999); // forward-compat unknown
        put_u32(&mut buf.0, 12, 8);
        put_u32(&mut buf.0, 16, TAG_END);
        put_u32(&mut buf.0, 20, 8);
        let info = BootInfo::parse(&buf.0).unwrap();
        assert!(matches!(info.tags().next(), Some(Tag::Other(999))));
    }

    #[test]
    fn rsdp_v2_preferred_over_v1() {
        // header (16) + v1 tag (24, padded to 24) + v2 tag (40, padded
        // to 40) + end (8) = 88
        let mut buf = Aligned::<96>([0u8; 96]);
        put_u32(&mut buf.0, 0, 88);
        // v1 RSDP tag at offset 8: size=8+20=28 (padded to 32)
        put_u32(&mut buf.0, 8, TAG_RSDP_V1);
        put_u32(&mut buf.0, 12, 28);
        buf.0[16] = 0xAA; // payload marker
                          // v2 RSDP tag at offset 40: size=8+36=44 (padded to 48)
        put_u32(&mut buf.0, 40, TAG_RSDP_V2);
        put_u32(&mut buf.0, 44, 44);
        buf.0[48] = 0xBB; // payload marker
                          // end tag at offset 88
        put_u32(&mut buf.0, 88, TAG_END);
        put_u32(&mut buf.0, 92, 8);
        // total_size includes end (8), so 88 + 8 = 96; fix header.
        put_u32(&mut buf.0, 0, 96);

        let info = BootInfo::parse(&buf.0).unwrap();
        let rsdp = info.rsdp().expect("rsdp present");
        assert_eq!(rsdp[0], 0xBB, "v2 must beat v1");
    }
}
