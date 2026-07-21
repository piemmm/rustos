//! Multiboot2 information-structure access for the x86_64 boot path.
//!
//! A real boot-loader (GRUB, or the first-party TAIRiX loader) enters the
//! kernel's `_start` with a pointer to a Multiboot2 information structure.
//! The wire layout of that structure — the header, the 8-byte-aligned tag
//! stream, and every tag this port understands (basic memory, memory map,
//! ACPI RSDP, EFI memory map, …) — is defined once in the shared
//! [`tairix_multiboot2`] crate, so the kernel *parser* and the loader
//! *builder* can never drift on what a tag means. This module re-exports
//! that shared surface under the historical `crate::multiboot2` path the
//! rest of the port (`bootinfo`, `bootmemory`) already uses.
//!
//! See [`tairix_multiboot2`] for the parser (`BootInfo`) and the loader's
//! `InfoBuilder`, and `plans/BOOTLOADER.md` for the boot chain that builds
//! the structure this parser reads.

pub use tairix_multiboot2::{
    BootInfo, EfiMemoryDescriptor, EfiMemoryEntryIter, EfiMemoryMap, FramebufferInfo,
    Mb2MemoryEntry, Mb2MemoryEntryIter, Mb2MemoryKind, MemoryMap, ParseError, Tag, TagIter,
};
