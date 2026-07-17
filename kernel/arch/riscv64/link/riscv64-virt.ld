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

SECTIONS
{
    . = 0x80200000;

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
