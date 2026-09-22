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
 *   - the boot heap has a declared `.heap` (NOLOAD) section, as on
 *     riscv64. Left undeclared it is an orphan, which the linker may
 *     place as PROGBITS (64 MiB of zeros in the image) or drop, leaving
 *     the allocator a base backing no RAM; `KEEP` holds it when only a
 *     `const` address-of refers to it.
 */

OUTPUT_ARCH(aarch64)
ENTRY(_start)

/* Boot-stack size for images built against this script. Sized by the
 * image, not by the shared trampoline: the QEMU `virt` images run the
 * determinism verticals, whose figure reference grid measured above
 * 64 KiB and below 128 KiB, so this is roughly twice the measured
 * ceiling. The Pi 4 script keeps the smaller boot-pipeline figure. */
BOOT_STACK_BYTES = 256K;

/* Poison guard reserved immediately below the boot stack. The MMU is off
 * on the boot CPU, so an overrun cannot be caught by unmapping a page:
 * the stub fills this gap with the shared sentinel instead and the panic
 * path reads it back. Sized per image like the stack above it. */
BOOT_STACK_GUARD_BYTES = 4K;

SECTIONS
{
    . = 0x40200000;
    __kernel_start = .;

    .text : ALIGN(4K)
    {
        __text_start = .;
        KEEP(*(.text.boot))
        *(.text .text.*)
        __text_end = .;
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
        /* Last in `.bss` on purpose. The MMU is off here, so no guard
         * page can catch an overrun; placing the stack anywhere lower
         * lets one run off the bottom of `.bss` into `.rodata`/`.text`,
         * where corrupting a vtable slot turns into a branch to data and
         * a silent hang. Growing down into the zeroed statics above
         * `__bss_start` keeps an overrun off the code. */
        . = ALIGN(4K);
        __boot_stack_guard_bottom = .;
        . += BOOT_STACK_GUARD_BYTES;
        /* The guard is every byte up to `__boot_stack_bottom`, alignment
         * padding included, so the stub fills exactly what the panic path
         * checks and no third symbol can drift between them. */
        . = ALIGN(16);
        __boot_stack_bottom = .;
        . += BOOT_STACK_BYTES;
        __boot_stack_top = .;
        . = ALIGN(16);
        __bss_end = .;
    }

    . = ALIGN(4K);
    .heap (NOLOAD) :
    {
        KEEP(*(.heap))
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
