# tairix-bootload

The firmware-neutral **loader core** for the TAIRiX boot chain
(`plans/BOOTLOADER.md`, increment B1).

Given the kernel ELF64 image, `plan_kernel_load` computes a validated
`LoadPlan`: the `PT_LOAD` segments to place (file source range, physical
destination, memory size with a zero-filled BSS tail, and read/write/execute
permissions), the entry point, and the physical span the firmware must have
free. This is the shared *decision* every TAIRiX loader makes; the
per-firmware shells under `boot/` (UEFI, legacy BIOS) perform the actual
placement.

The crate is `no_std`, allocation-free (the segment list is a fixed,
bounded array — a security limit on an untrusted image, not a growable
capacity), and forbids `unsafe`. It touches no hardware and places nothing.
It decodes the image through `tairix-binfmt` so the load path and the
executable-inspection path can never disagree on what an ELF field means.

Every field is bounds- and shape-checked before it is trusted: a malformed
or hostile image (bad ELF, non-executable, wrong machine, no loadable
segment, a file range past the image, a file larger than its memory image,
a misaligned segment, overlapping destinations, a write-executable segment,
or more segments than the cap) is a typed `LoadError`, never a panic and
never a partial trust of later bytes.

## Stability

**experimental.** The loader-core API is unfrozen while the boot chain is
built out (`plans/BOOTLOADER.md`); it will settle as the UEFI and BIOS
shells land on top of it.
