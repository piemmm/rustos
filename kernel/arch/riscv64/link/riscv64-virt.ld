/* Linker script for TAIRiX riscv64 kernels on the QEMU `virt` board.
 *
 * OpenSBI (`-bios default`) occupies the bottom of RAM at 0x80000000
 * and jumps to the `-kernel` ELF's entry point in S-mode. The
 * conventional S-mode payload load address on the `virt` board is
 * 0x80200000 (2 MiB above RAM base), which leaves OpenSBI its
 * firmware region below.
 *
 * Invariants enforced here:
 *   - `.text.boot` (the `_start` trampoline) is the first sub-section
 *     of the first PT_LOAD segment, so the ELF entry point is the
 *     trampoline.
 *   - `__bss_start` / `__bss_end` bracket the zero-init region and are
 *     8-byte aligned for the doubleword-store clear loop in `boot.s`.
 *   - The 64 MiB boot heap lives in a separate `.heap` (NOLOAD)
 *     section placed *after* `__bss_end`, so the boot trampoline does
 *     not waste cycles zeroing it (the bump allocator does not need
 *     zeroed backing) and the boot pipeline can exclude it from the
 *     usable physical-memory map by starting the usable region at
 *     `__kernel_end`.
 *   - The default `medany` code model means every reference is
 *     pc-relative; no `__global_pointer$` is defined, so the linker
 *     never relaxes a load to gp-relative addressing (which would
 *     require runtime `gp` setup the boot stub deliberately omits).
 */

OUTPUT_ARCH(riscv)
ENTRY(_start)

/* Boot-stack size for images built against this script. Sized by the
 * image, not by the shared trampoline: these images run the determinism
 * verticals, whose figure reference grid measured above 64 KiB and below
 * 128 KiB, so this is roughly twice the measured ceiling. */
BOOT_STACK_BYTES = 256K;

/* Poison guard reserved immediately below the boot stack. The MMU is off
 * on the boot hart, so an overrun cannot be caught by unmapping a page:
 * the stub fills this gap with the shared sentinel instead and the panic
 * path reads it back. Sized per image like the stack above it. */
BOOT_STACK_GUARD_BYTES = 4K;

SECTIONS
{
    . = 0x80200000;
    __kernel_image_start = .;

    .text : ALIGN(4K)
    {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }

    .rodata : ALIGN(4K)
    {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    }

    .data : ALIGN(4K)
    {
        *(.data .data.*)
        *(.sdata .sdata.*)
        *(.got .got.*)
    }

    .bss : ALIGN(8)
    {
        __bss_start = .;
        *(.sbss .sbss.*)
        *(.bss .bss.*)
        *(COMMON)
        /* Last in `.bss` on purpose. The MMU is off here, so no guard
         * page can catch an overrun; placing the stack anywhere lower
         * lets one run off the bottom of `.bss` into `.rodata`/`.text`.
         * Growing down into the zeroed statics above `__bss_start` keeps
         * an overrun off the code. */
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
        . = ALIGN(8);
        __bss_end = .;
    }

    . = ALIGN(4K);
    .heap (NOLOAD) :
    {
        *(.heap)
    }

    . = ALIGN(4K);
    __kernel_end = .;

    /DISCARD/ :
    {
        *(.eh_frame .eh_frame.*)
        *(.eh_frame_hdr)
        *(.note .note.*)
        *(.comment)
        *(.riscv.attributes)
    }
}
