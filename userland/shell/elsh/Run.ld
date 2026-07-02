/* Link script for the `Shell` bundle `Run` binary
 * (`userland/shell/elsh/src/run.rs`).
 *
 * The `Run` program is a position-independent **pure-Rust** program whose
 * entry point is the `rustos-rt` runtime's `_start`. It links no architecture
 * crate, so nothing in the default object graph references `_start`; this
 * script makes `_start` the entry GC root and lays the loadable sections out
 * at a fixed, non-zero base so the consuming boot path maps the image at its
 * link addresses with the chosen relocation bias (`AGENTS.md` §9, §19.2 — the
 * image is genuinely PIE). One script serves every native target: it names
 * only architecture-neutral section classes (`AGENTS.md` §2.2). It is the
 * same proven PIE layout `userland/system/init/Run.ld` uses; the two are kept
 * byte-for-byte identical so the userland program runtimes share one layout.
 *
 * Every loadable segment is page-aligned in both file offset and virtual
 * address: the kernel `rxe` loader maps whole pages with per-segment
 * permissions, so a `PT_LOAD` whose virtual address is not a multiple of the
 * page size is refused (W^X page granularity, §19.2). The explicit
 * `. = ALIGN(CONSTANT(MAXPAGESIZE))` between permission groups — together with
 * naming the PIE metadata sections so no orphan lands mid-page — is what keeps
 * each segment on its own page boundary. */

ENTRY(_start)

SECTIONS
{
    /* A page-aligned, non-zero base keeps the null page unmapped and gives the
     * loader clean page boundaries to map at. */
    . = 0x10000;

    /* Executable code (R+X). */
    .text :
    {
        KEEP(*(.text._start))
        *(.text .text.*)
    }

    /* Read-only data and the PIE dynamic-linking metadata (R). Page-align the
     * start so this becomes its own R-only PT_LOAD rather than sharing the
     * code page. The dynamic sections are named explicitly so the linker does
     * not place them as orphans at a non-page-aligned address. */
    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .rodata :
    {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    }
    .gnu.hash       : { *(.gnu.hash) }
    .hash           : { *(.hash) }
    .dynsym         : { *(.dynsym) }
    .dynstr         : { *(.dynstr) }
    .gnu.version    : { *(.gnu.version) }
    .gnu.version_r  : { *(.gnu.version_r) }
    .rela.dyn       : { *(.rela.dyn) }

    /* Writable data, the GOT, and the dynamic table (R+W). */
    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .data :
    {
        *(.data .data.*)
        *(.sdata .sdata.*)
        *(.got .got.*)
        *(.got.plt)
    }
    .dynamic : { *(.dynamic) }

    /* Zero-initialised data (R+W, not file-backed). */
    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .bss (NOLOAD) :
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
