/* Link script for the CCOMPAT CC3 fixture program (`tests/integration/cc3_program`).
 *
 * The program is a position-independent C-ABI executable whose entry point is
 * crt0's `_start` (`rustos_crt0`). It links no architecture crate, so nothing
 * in the default object graph references `_start`; this script makes `_start`
 * the entry GC root and lays the loadable sections out at a fixed, non-zero
 * base so the consuming test maps the image at its link addresses with a zero
 * relocation bias (`AGENTS.md` §9, §19.2 — the image is genuinely PIE; the
 * test exercises it at its preferred base). One script serves every native
 * target: it only names architecture-neutral section classes (`AGENTS.md`
 * §2.2). */

ENTRY(_start)

SECTIONS
{
    /* A page-aligned, non-zero base keeps the null page unmapped and gives the
     * loader clean page boundaries to map at. */
    . = 0x10000;

    .text : ALIGN(0x1000)
    {
        KEEP(*(.text._start))
        *(.text .text.*)
    }

    .rodata : ALIGN(0x1000)
    {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    }

    .data : ALIGN(0x1000)
    {
        *(.data .data.*)
        *(.sdata .sdata.*)
        *(.got .got.*)
        *(.got.plt)
    }

    .bss (NOLOAD) : ALIGN(0x1000)
    {
        *(.bss .bss.*)
        *(.sbss .sbss.*)
        *(COMMON)
    }

    /* Metadata that never loads into the running image. */
    /DISCARD/ :
    {
        *(.eh_frame .eh_frame_hdr)
        *(.comment)
        *(.note .note.*)
        *(.riscv.attributes)
        *(.ARM.attributes)
    }
}
