//! Borrow-only, allocation-free parser for the Multiboot2 information
//! structure the boot-loader hands the kernel.
//!
//! Nothing here allocates; the parser validates that every tag is fully
//! contained in the input slice and refuses (returns `Err`) on truncation,
//! mis-alignment, or overflow. Higher-level consumers translate the typed
//! tag stream into the kernel's own memory-map / ACPI structures.

use core::mem::size_of;
use core::str;

use crate::{
    TAG_BASIC_MEMORY, TAG_CMDLINE, TAG_EFI_MMAP, TAG_END, TAG_FRAMEBUFFER, TAG_MMAP, TAG_RSDP_V1,
    TAG_RSDP_V2,
};

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
/// 8 bytes: `total_size: u32`, `reserved: u32`, then tags.
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
        if !(buf.as_ptr() as usize).is_multiple_of(8) {
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
        // (truncated tags, missing end tag). This lets every downstream
        // iterator be infallible.
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

    /// Convenience: the boot command line (Multiboot2 type 1), if present
    /// and valid UTF-8.
    #[must_use]
    pub fn command_line(&self) -> Option<&'a str> {
        for tag in self.tags() {
            if let Tag::CommandLine(s) = tag {
                return Some(s);
            }
        }
        None
    }

    /// Convenience: the framebuffer descriptor (Multiboot2 type 8), if
    /// present.
    #[must_use]
    pub fn framebuffer(&self) -> Option<FramebufferInfo> {
        for tag in self.tags() {
            if let Tag::Framebuffer(fb) = tag {
                return Some(fb);
            }
        }
        None
    }
}

/// One recognised tag from the Multiboot2 stream.
#[derive(Debug, Clone, Copy)]
pub enum Tag<'a> {
    /// Tag types we accept but expose nothing for (forward compat).
    Other(u32),
    /// Multiboot2 type 1 — the boot command line (valid UTF-8; a tag whose
    /// bytes are not valid UTF-8 degrades to [`Tag::Other`]).
    CommandLine(&'a str),
    /// Multiboot2 type 4 — `mem_lower` and `mem_upper`.
    BasicMemory {
        /// Lower memory in KiB (640 KiB conventional region).
        lower_kib: u32,
        /// Upper memory in KiB starting at 1 MiB.
        upper_kib: u32,
    },
    /// Multiboot2 type 6 — BIOS-derived memory map.
    MemoryMap(MemoryMap<'a>),
    /// Multiboot2 type 8 — framebuffer information.
    Framebuffer(FramebufferInfo),
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
        // bounds. We re-check defensively anyway (cheap, and no "trust me").
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
        TAG_CMDLINE => decode_cmdline(payload).map_or(Tag::Other(tag_type), Tag::CommandLine),
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
        TAG_FRAMEBUFFER => {
            FramebufferInfo::decode(payload).map_or(Tag::Other(tag_type), Tag::Framebuffer)
        }
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

/// Decode a command-line payload: the bytes up to the first NUL, as UTF-8.
/// A payload with no NUL terminator, or whose leading bytes are not valid
/// UTF-8, is rejected (the caller degrades it to [`Tag::Other`]).
fn decode_cmdline(payload: &[u8]) -> Option<&str> {
    let nul = payload.iter().position(|&b| b == 0)?;
    str::from_utf8(&payload[..nul]).ok()
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
    /// Decode the raw Multiboot2 `type` field.
    #[must_use]
    pub fn from_raw(v: u32) -> Self {
        match v {
            1 => Self::Available,
            3 => Self::AcpiReclaimable,
            4 => Self::AcpiNvs,
            5 => Self::Defective,
            _ => Self::Reserved,
        }
    }

    /// The canonical raw Multiboot2 `type` field for this kind. Inverse of
    /// [`Mb2MemoryKind::from_raw`] for the canonical encodings (the builder
    /// emits these values; [`Mb2MemoryKind::Reserved`] uses `2`, the
    /// spec's generic "reserved" code).
    #[must_use]
    pub fn to_raw(self) -> u32 {
        match self {
            Self::Available => 1,
            Self::Reserved => 2,
            Self::AcpiReclaimable => 3,
            Self::AcpiNvs => 4,
            Self::Defective => 5,
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
        if !body.len().is_multiple_of(entry_size) {
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

// --- Framebuffer (type 8) --------------------------------------------

/// Framebuffer descriptor (Multiboot2 type 8), decoded for a direct-RGB
/// colour framebuffer. The colour-field members are meaningful only when
/// `fb_type == `[`crate::FRAMEBUFFER_TYPE_RGB`]; for any other framebuffer
/// type they are zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramebufferInfo {
    /// Physical address of the framebuffer.
    pub addr: u64,
    /// Bytes per scan line.
    pub pitch: u32,
    /// Width in pixels (or characters for an EGA-text framebuffer).
    pub width: u32,
    /// Height in pixels (or characters for an EGA-text framebuffer).
    pub height: u32,
    /// Bits per pixel.
    pub bpp: u8,
    /// Framebuffer type (`1` == direct RGB, see
    /// [`crate::FRAMEBUFFER_TYPE_RGB`]).
    pub fb_type: u8,
    /// Bit position of the least-significant red bit.
    pub red_field_position: u8,
    /// Number of red bits.
    pub red_mask_size: u8,
    /// Bit position of the least-significant green bit.
    pub green_field_position: u8,
    /// Number of green bits.
    pub green_mask_size: u8,
    /// Bit position of the least-significant blue bit.
    pub blue_field_position: u8,
    /// Number of blue bits.
    pub blue_mask_size: u8,
}

impl FramebufferInfo {
    /// Fixed common-header length of a framebuffer tag payload: `addr`
    /// (8) + `pitch` (4) + `width` (4) + `height` (4) + `bpp` (1) +
    /// `fb_type` (1) + `reserved` (2).
    pub(crate) const COMMON_LEN: usize = 24;
    /// Length of the direct-RGB colour-info block that follows the common
    /// header: six `u8` field-position / mask-size values.
    pub(crate) const RGB_COLOR_LEN: usize = 6;

    fn decode(payload: &[u8]) -> Option<Self> {
        if payload.len() < Self::COMMON_LEN {
            return None;
        }
        let addr = u64::from_le_bytes([
            payload[0], payload[1], payload[2], payload[3], payload[4], payload[5], payload[6],
            payload[7],
        ]);
        let pitch = u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]);
        let width = u32::from_le_bytes([payload[12], payload[13], payload[14], payload[15]]);
        let height = u32::from_le_bytes([payload[16], payload[17], payload[18], payload[19]]);
        let bpp = payload[20];
        let fb_type = payload[21];
        // payload[22..24] is reserved.
        let mut fb = FramebufferInfo {
            addr,
            pitch,
            width,
            height,
            bpp,
            fb_type,
            red_field_position: 0,
            red_mask_size: 0,
            green_field_position: 0,
            green_mask_size: 0,
            blue_field_position: 0,
            blue_mask_size: 0,
        };
        if fb_type == crate::FRAMEBUFFER_TYPE_RGB
            && payload.len() >= Self::COMMON_LEN + Self::RGB_COLOR_LEN
        {
            let c = &payload[Self::COMMON_LEN..];
            fb.red_field_position = c[0];
            fb.red_mask_size = c[1];
            fb.green_field_position = c[2];
            fb.green_mask_size = c[3];
            fb.blue_field_position = c[4];
            fb.blue_mask_size = c[5];
        }
        Some(fb)
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
        if !body.len().is_multiple_of(descriptor_size) {
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
