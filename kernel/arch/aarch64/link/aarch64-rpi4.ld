/* Linker script for TAIRiX aarch64 kernels on the Raspberry Pi 4 (BCM2711).
 *
 * The Raspberry Pi firmware (`start4.elf`) loads the AArch64 kernel image
 * `kernel8.img` at physical address 0x8_0000 (512 KiB) and enters it at
 * EL2 with the Linux aarch64 boot-protocol hand-off:
 *
 *   x0 = physical address of the flattened device tree (DTB)
 *   x1..x3 = 0 (reserved)
 *
 * See `docs/src/platform/aarch64.md` ("Raspberry Pi 4 (BCM2711)") for the
 * authoritative boot-protocol facts this address derives from.
 *
 * This is the per-board boot artefact the `AGENTS.md` §1 "boot stubs"
 * carve-out permits: it is identical to `aarch64-virt.ld` save for the
 * load address (and this comment), because the load address is fixed
 * before any device tree is parsed and so cannot be discovered at runtime
 * (`plans/PI.md` §0.2). It is *not* a `cfg(board=…)` fork of the port —
 * every other board difference is discovered device-tree data
 * (`plans/PI.md` §0.3).
 *
 * Invariants enforced here (identical to `aarch64-virt.ld`):
 *   - `.text.boot` (the `_start` trampoline) is the first sub-section
 *     of the first PT_LOAD segment, so the ELF entry point is the
 *     trampoline.
 *   - `__bss_start` / `__bss_end` bracket the zero-init region and are
 *     16-byte aligned for the 16-byte-store clear loop in `boot.s`.
 *   - the exception vector table lands on its required 2 KiB alignment
 *     (`VBAR_EL1` ignores the low 11 bits); it lives in `.text` and the
 *     vector module's own `.balign 0x800` keeps it aligned.
 */

OUTPUT_ARCH(aarch64)
ENTRY(_start)

SECTIONS
{
    . = 0x00080000;
    __kernel_start = .;

    .text : ALIGN(4K)
    {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }

    .rodata : ALIGN(4K)
    {
        *(.rodata .rodata.*)
    }

    .data : ALIGN(4K)
    {
        *(.data .data.*)
        *(.got .got.*)
    }

    .bss : ALIGN(16)
    {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }

    . = ALIGN(4K);
    __kernel_end = .;

    /DISCARD/ :
    {
        *(.eh_frame .eh_frame.*)
        *(.eh_frame_hdr)
        *(.note .note.*)
        *(.comment)
    }
}
