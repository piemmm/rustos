/* Linker script for TAIRiX aarch64 kernels on the QEMU `virt` board.
 *
 * QEMU `-M virt` places guest RAM at 0x4000_0000. With `-kernel <elf>`
 * QEMU's ARM boot loader loads the ELF at its own physical addresses
 * and enters the ELF entry point with the firmware hand-off registers
 * set by the Linux aarch64 boot protocol:
 *
 *   x0 = physical address of the flattened device tree (DTB)
 *   x1..x3 = 0 (reserved)
 *
 * The conventional kernel load address on the `virt` board is 2 MiB
 * above RAM base (0x4020_0000), which leaves the low RAM for the DTB
 * and any firmware structures QEMU plants.
 *
 * Invariants enforced here:
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
    . = 0x40200000;
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
