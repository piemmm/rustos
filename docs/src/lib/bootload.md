# `tairix-bootload` — boot-chain loader core

`tairix_bootload` (`lib/bootload`) is the firmware-neutral heart of the
TAIRiX boot chain (`plans/BOOTLOADER.md`, increment B1): the one computation
that turns the kernel ELF64 image into a validated plan for placing it in
physical memory. A TAIRiX loader — the UEFI application, the legacy-BIOS
stub — must decide *what* to load before it can *do* the loading; that
decision is identical on every firmware, so it lives here once and every
per-firmware `boot/*` shell reuses it. Two loaders can never disagree on how
the kernel is laid out.

Stability tier: **experimental** (the surface settles as the UEFI and BIOS
shells land on top of it).

## What it computes

`plan_kernel_load(image, expected_machine)` decodes the image through the
shared `tairix_binfmt::elf` view and returns a `LoadPlan`:

- the `LoadSegment`s — one per `PT_LOAD` program header — each carrying the
  file source range (`file_offset`, `file_size`), the physical destination
  (`phys_dest`, from `p_paddr`), the total in-memory size (`mem_size`, whose
  tail past the file bytes is zero-filled — the BSS), and the
  read/write/execute permission flags decoded from `p_flags`;
- the entry point (`e_entry`) control transfers to once every segment is
  placed;
- the physical span (`phys_span`) — the lowest destination and the highest
  end across all segments — the firmware must have free.

`expected_machine` is the instruction set the calling firmware runs, so the
same core serves x86_64, aarch64, and riscv64 without naming one; it never
couples to a board or SoC.

## What it refuses

The image is untrusted, so the plan is computed only after the whole image
validates, and any of the following rejects it whole with a typed
`LoadError` — never a panic, never a partial trust:

- not a well-formed ELF64 (the shared decoder's error is wrapped, not
  hidden);
- not an `ET_EXEC` executable, or built for a different machine;
- no loadable segment, or more than the fixed cap (`MAX_LOAD_SEGMENTS`) —
  a security bound on a hostile image, not a growable capacity;
- a file range that runs past the image, or a file larger than its memory
  image;
- a zero-size segment, an address-space overflow, a non-power-of-two
  alignment, or a destination off its file offset's alignment residue;
- two segments whose destinations overlap;
- a segment that is both writable and executable (write-xor-execute is
  enforced before any byte is placed).

## Posture

The crate is `no_std`, forbids `unsafe`, and allocates nothing (the segment
list is a fixed array), so it builds in a firmware environment with no heap.
It touches no hardware and places nothing: it produces the plan the shell
then carries out. Because it decodes through `tairix-binfmt`, the load path
and the executable-inspection path can never drift on what an ELF field
means.
