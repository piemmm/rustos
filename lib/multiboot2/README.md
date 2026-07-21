# tairix-multiboot2

The shared **Multiboot2 information-structure wire layout** for the TAIRiX
boot chain (`plans/BOOTLOADER.md`, increment B2).

The Multiboot2 specification (rev 2.0) defines the tag stream a boot-loader
hands to the kernel. TAIRiX has two halves of the boot handoff that must
agree on that layout to the byte:

- the kernel **parses** the structure a loader placed in memory
  (`BootInfo` — borrow-only, allocation-free, fail-closed), and
- the first-party TAIRiX loader **builds** it into a caller-provided buffer
  before it enters the kernel (`InfoBuilder`).

This crate is the one definition both depend on, so the producer and the
consumer can never drift on what a tag means. The round-trip host tests
assemble a structure with the builder and read it back with the parser,
proving the two halves agree.

## Tags

Parsed: end (0), boot command line (1), basic memory (4), memory map (6),
framebuffer (8), ACPI 1.0 / 2.0 RSDP (14 / 15), EFI memory map (17).
Unknown tag types surface as `Tag::Other` and are skipped (forward
compatible).

Built: end, command line, basic memory, memory map, RSDP, framebuffer. The
loader has no reason to synthesise an EFI memory map (firmware supplies its
own memory map, which the loader forwards as a type-6 map), so the builder
does not emit tag 17.

## Guarantees

`no_std`, allocation-free, and `#![forbid(unsafe_code)]`. The parser
validates that every tag is fully contained in the input and refuses on
truncation, mis-alignment, or overflow — a malformed stream is a typed
`ParseError`, never a panic. The builder is fail-closed: an append that
would overflow the buffer or a length field, or a command line with an
interior NUL, is a typed `BuildError`, and the structure is left untrusted.

## Stability

**experimental.** The wire-layout API is unfrozen while the boot chain is
built out (`plans/BOOTLOADER.md`); the parse side has one in-tree consumer
today (the kernel x86_64 port) and the build side lands its consumers with
the UEFI (B3) and BIOS (B5) loader shells.
