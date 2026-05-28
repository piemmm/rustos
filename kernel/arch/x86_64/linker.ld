/* Linker script for RustOS x86_64 multiboot1 kernels.
 *
 * Used by every QEMU integration test binary in `tests/integration/*` that
 * targets `x86_64-unknown-none`. The script is the *only* place the boot
 * layout is encoded; AGENTS.md §2.2 forbids duplicating it per-test.
 *
 * Invariants enforced here:
 *   - The multiboot1 header is placed in the first sub-section of the first
 *     PT_LOAD segment, well within the 8 KiB QEMU scans (`boot.s`
 *     SAFETY-INVARIANT 1).
 *   - The .boot.text (32-bit trampoline) follows the header so the entry
 *     point lives at the lowest loadable address.
 *   - All other sections come after, so a 32-bit `EAX = absolute_addr`
 *     reference in the trampoline fits in 32 bits (kernel is loaded at 1 MiB
 *     and is far smaller than 4 GiB).
 *   - Unwind / debug-only sections that the multiboot loader doesn't need
 *     are discarded so the kernel ELF stays small.
 */

ENTRY(_start)
OUTPUT_FORMAT(elf64-x86-64)

SECTIONS
{
    . = 1M;

    .boot ALIGN(4K) :
    {
        KEEP(*(.multiboot_header))
        *(.boot.text)
    }

    .text ALIGN(4K) :
    {
        *(.text .text.*)
    }

    .rodata ALIGN(4K) :
    {
        *(.rodata .rodata.*)
    }

    .data ALIGN(4K) :
    {
        *(.data .data.*)
        *(.got .got.*)
        *(.got.plt)
    }

    .bss ALIGN(4K) :
    {
        *(.bss .bss.*)
        *(COMMON)
    }

    /DISCARD/ :
    {
        *(.eh_frame .eh_frame.*)
        *(.eh_frame_hdr)
        *(.note .note.*)
        *(.comment)
        *(.gnu.build-id)
        *(.gnu.hash)
        *(.gnu.version*)
        *(.dynamic)
        *(.dynsym)
        *(.dynstr)
        *(.rela .rela.*)
        *(.interp)
        *(.hash)
    }
}
