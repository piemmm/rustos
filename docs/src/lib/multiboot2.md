# `tairix-multiboot2` — shared Multiboot2 wire layout

`tairix_multiboot2` (`lib/multiboot2`) is the one definition of the
Multiboot2 information-structure wire layout (specification rev 2.0), shared
by the two halves of the TAIRiX boot handoff (`plans/BOOTLOADER.md`,
increment B2):

- the kernel **parses** the structure a boot-loader placed in memory
  (`BootInfo`), and
- the first-party TAIRiX loader **builds** it into a buffer before entering
  the kernel (`InfoBuilder`).

Because both depend on this crate, the producer and consumer can never drift
on what a tag means. The round-trip host tests assemble a structure with the
builder and read it back with the parser, proving the two agree.

Stability tier: **experimental** (the surface settles as the UEFI and BIOS
loader shells land on top of it).

## The wire format

A Multiboot2 information structure is an 8-byte header (`total_size`,
`reserved`) followed by 8-byte-aligned tags, terminated by an end tag (type
`0`, size `8`). Each tag starts with a `type: u32` and a `size: u32`
(header-inclusive), then a tag-specific payload.

## Parsing (`BootInfo`)

`BootInfo::parse(buf)` validates the whole stream up front — the buffer must
be 8-byte aligned, `total_size` must be consistent, every tag must be fully
contained, and the stream must end with an end tag — and returns a
borrow-only view. It allocates nothing. A structural defect is a typed
`ParseError` (`Misaligned`, `HeaderTruncated`, `HeaderInconsistent`,
`TagTruncated`), never a panic.

The view iterates typed `Tag`s and offers convenience accessors:
`memory_map()`, `efi_memory_map()`, `rsdp()` (ACPI 2.0 preferred over 1.0),
`command_line()`, and `framebuffer()`. Recognised tags: boot command line
(1), basic memory (4), memory map (6), framebuffer (8), ACPI RSDP (14 / 15),
and EFI memory map (17). An unknown tag type surfaces as `Tag::Other` and is
skipped, so the parser is forward-compatible with future revisions; a
malformed *recognised* tag (e.g. a memory map with a sub-minimum entry size)
also degrades to `Tag::Other` rather than tearing down the whole stream.

## Building (`InfoBuilder`)

`InfoBuilder::new(buf)` starts assembling into a caller-provided buffer;
`basic_memory`, `command_line`, `memory_map`, `rsdp`, and `framebuffer`
append tags; `finish` writes the terminating end tag, back-fills
`total_size`, and returns the written bytes (which the loader must place on
an 8-byte boundary). The builder is allocation-free and fail-closed: an
append that would overflow the buffer or a length field, or a command line
with an interior NUL, is a typed `BuildError` and the structure is left
untrusted. The reservation always keeps room for the mandatory end tag, so
`finish` cannot fail.

The builder does not emit an EFI memory map (type 17): firmware supplies its
own memory map, which the loader forwards as a type-6 map.

## Kernel consumer

The x86_64 port re-exports the parser under `crate::multiboot2`
(`kernel/arch/x86_64/src/multiboot2.rs`) and turns the typed tags into the
kernel's own `BootMemoryMap` and ACPI RSDP pointers (`bootmemory`,
`bootinfo`). QEMU's PVH direct boot uses a separate path (`pvh`); the
Multiboot2 path is what a real boot-loader (GRUB, or the first-party TAIRiX
loader) drives.
