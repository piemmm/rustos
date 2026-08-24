//! Shared Multiboot2 information-structure wire layout.
//!
//! The Multiboot2 specification (rev 2.0) defines the tag stream a
//! boot-loader hands to the kernel: a 16-byte information header
//! (`total_size`, `reserved`) followed by a sequence of 8-byte-aligned
//! tags. Each tag begins with a 4-byte `type`, a 4-byte `size` (header
//! inclusive), and a tag-specific payload. The stream is terminated by a
//! tag of type `0` and size `8`.
//!
//! TAIRiX has two halves of the boot handoff that must agree on this
//! layout to the byte: the kernel [`BootInfo`] *parser* (borrow-only,
//! allocation-free, fail-closed) reads whatever a loader placed in memory,
//! and the first-party loader's [`InfoBuilder`] *builder* assembles the
//! same structure into a caller-provided buffer before it enters the
//! kernel. This crate is the one definition both depend on, so the
//! producer and consumer can never drift on what a tag means (they are
//! proven to agree by the round-trip host tests).
//!
//! # Tags this crate understands
//!
//! | Type | Name              | Parse | Build |
//! |-----:|-------------------|:-----:|:-----:|
//! |   0  | End               | yes   | yes   |
//! |   1  | Boot command line | yes   | yes   |
//! |   4  | Basic memory      | yes   | yes   |
//! |   6  | Memory map        | yes   | yes   |
//! |   8  | Framebuffer       | yes   | yes   |
//! |  14  | ACPI 1.0 RSDP     | yes   | yes   |
//! |  15  | ACPI 2.0 RSDP     | yes   | yes   |
//! |  17  | EFI memory map    | yes   | no    |
//!
//! Unknown tag types are *not* an error on the parse side: they surface as
//! [`Tag::Other`] and are skipped, so the parser is forward-compatible with
//! future Multiboot2 revisions.
//!
//! # References
//!
//! * Multiboot2 specification, rev 2.0 ("Boot information format").
//! * UEFI 2.10, §7.2 ("Memory Allocation Services") — the EFI descriptor
//!   layout carried by tag 17.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
// The wire structures carry the spec's own names (`Multiboot2Header`,
// `Multiboot2Tag`), which a reader matching them against the specification
// needs spelled in full.
#![allow(clippy::module_name_repetitions)]

mod build;
mod parse;

pub use build::{BuildError, InfoBuilder};
pub use parse::{
    BootInfo, EfiMemoryDescriptor, EfiMemoryEntryIter, EfiMemoryMap, FramebufferInfo,
    Mb2MemoryEntry, Mb2MemoryEntryIter, Mb2MemoryKind, MemoryMap, ParseError, Tag, TagIter,
};

#[cfg(test)]
mod tests;

/// Multiboot2 tag type: stream terminator.
pub(crate) const TAG_END: u32 = 0;
/// Multiboot2 tag type: boot command line (a null-terminated string).
pub(crate) const TAG_CMDLINE: u32 = 1;
/// Multiboot2 tag type: basic memory (`mem_lower` / `mem_upper`).
pub(crate) const TAG_BASIC_MEMORY: u32 = 4;
/// Multiboot2 tag type: BIOS-derived memory map.
pub(crate) const TAG_MMAP: u32 = 6;
/// Multiboot2 tag type: framebuffer information.
pub(crate) const TAG_FRAMEBUFFER: u32 = 8;
/// Multiboot2 tag type: ACPI 1.0 RSDP (20-byte descriptor).
pub(crate) const TAG_RSDP_V1: u32 = 14;
/// Multiboot2 tag type: ACPI 2.0+ RSDP (36-byte, XSDT-capable descriptor).
pub(crate) const TAG_RSDP_V2: u32 = 15;
/// Multiboot2 tag type: EFI memory map (raw `EFI_MEMORY_DESCRIPTOR`s).
pub(crate) const TAG_EFI_MMAP: u32 = 17;

/// Size of the 8-byte tag header (`type: u32`, `size: u32`) prefixing
/// every tag, and of the 8-byte information header (`total_size: u32`,
/// `reserved: u32`) prefixing the whole stream. Both are 8 bytes.
pub(crate) const HEADER_BYTES: usize = 8;

/// Framebuffer `framebuffer_type` value for a direct-RGB colour framebuffer
/// (Multiboot2 spec §3.6.12, `framebuffer_type == 1`).
pub const FRAMEBUFFER_TYPE_RGB: u8 = 1;

/// Round `n` up to the next multiple of 8 (Multiboot2 tags are 8-byte
/// aligned in the stream). Returns `None` on overflow.
pub(crate) fn align8(n: usize) -> Option<usize> {
    Some(n.checked_add(7)? & !7)
}

/// Write a little-endian `u32` at `off` in `buf`. The caller guarantees
/// `off + 4 <= buf.len()`.
pub(crate) fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// Write a little-endian `u64` at `off` in `buf`. The caller guarantees
/// `off + 8 <= buf.len()`.
pub(crate) fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
