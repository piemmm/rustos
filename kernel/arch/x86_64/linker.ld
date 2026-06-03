/* Linker script for RustOS x86_64 multiboot2 kernels (higher-half).
 *
 * Used by every QEMU integration test binary in `tests/integration/*` that
 * targets `x86_64-unknown-none`. The script is the *only* place the boot
 * layout is encoded; AGENTS.md §2.2 forbids duplicating it per-test.
 *
 * # Higher-half memory model
 *
 * The kernel is a -2 GiB higher-half kernel: every Rust section
 * (.text/.rodata/.data/.bss and the AP-trampoline payload) is linked at a
 * *virtual* address in `[KERNEL_VMA_BASE, 0)` but *loaded* (LMA) into low
 * physical memory just above 1 MiB. This is what `-C code-model=kernel`
 * (`.cargo/config.toml`) already assumes, and what lets the syscall-entry
 * RSP0 validator (`syscall_entry::validate_kernel_rsp0`) demand a
 * canonical higher-half kernel stack (ret2usr / CVE-2019-1125 hardening,
 * AGENTS.md §5).
 *
 * The early boot trampoline (`.boot.*`) is the one region that must stay at
 * its physical load address: it executes in 32-bit protected mode with
 * paging off, so it is linked 1:1 (VMA == LMA) in low memory. It builds the
 * page tables that map BOTH the low 0..4 GiB identity window (preserved for
 * the direct physical map the DMA/MMIO seam relies on — `kernel/mem` phys.rs)
 * AND the higher-half kernel window, then jumps to the high half.
 *
 * Invariants enforced here:
 *   - The multiboot2 header is placed in the first sub-section of the first
 *     PT_LOAD segment, well within the 32 KiB the loader scans (`boot.s`
 *     SAFETY-INVARIANT 1).
 *   - `.boot.text` (the 32-bit trampoline) follows the header so the entry
 *     point lives at the lowest loadable address, and is linked 1:1.
 *   - The boot page tables, boot stack, and early GDT live in `.boot.bss` /
 *     `.boot.rodata`, also linked 1:1, so the 32-bit trampoline can name
 *     them with absolute 32-bit operands before paging is on.
 *   - All other sections are linked at `KERNEL_VMA_BASE + phys` with an
 *     explicit `AT()` mapping their LMA back to the contiguous low physical
 *     image.
 *   - Unwind / debug-only sections the loader doesn't need are discarded.
 */

ENTRY(_start)
OUTPUT_FORMAT(elf64-x86-64)

/* Top of the canonical higher half: the -2 GiB kernel window base. A kernel
 * symbol at virtual `KERNEL_VMA_BASE + p` is loaded at physical `p`. */
KERNEL_VMA_BASE = 0xFFFFFFFF80000000;

SECTIONS
{
    . = 1M;
    __boot_phys_start = .;

    .boot ALIGN(4K) :
    {
        KEEP(*(.multiboot_header))
        *(.boot.text)
    }

    .boot.rodata ALIGN(4K) :
    {
        *(.boot.rodata)
    }

    .boot.bss ALIGN(4K) :
    {
        *(.boot.bss)
    }

    . = ALIGN(4K);
    __boot_phys_end = .;

    /* Switch the location counter (VMA) into the higher half. Each section
     * below maps its LMA back to the contiguous low physical image with an
     * explicit `AT(ADDR(section) - KERNEL_VMA_BASE)`. */
    . += KERNEL_VMA_BASE;

    .text ALIGN(4K) : AT(ADDR(.text) - KERNEL_VMA_BASE)
    {
        *(.text .text.*)
    }

    /* AP startup payload (Stage 3a (b)). Embedded into the kernel image
     * as opaque bytes; the BSP copies `[_ap_trampoline_start,
     * _ap_trampoline_end)` to the chosen 4 KiB-aligned low physical frame
     * before INIT-SIPI-SIPI. The payload is position-independent within its
     * own page, so its higher-half link address only matters for the BSP's
     * `copy_nonoverlapping` source read (reachable via the kernel map). */
    .ap_trampoline ALIGN(4K) : AT(ADDR(.ap_trampoline) - KERNEL_VMA_BASE)
    {
        KEEP(*(.ap_trampoline))
    }

    .rodata ALIGN(4K) : AT(ADDR(.rodata) - KERNEL_VMA_BASE)
    {
        *(.rodata .rodata.*)
    }

    .data ALIGN(4K) : AT(ADDR(.data) - KERNEL_VMA_BASE)
    {
        *(.data .data.*)
        *(.got .got.*)
        *(.got.plt)
    }

    .bss ALIGN(4K) : AT(ADDR(.bss) - KERNEL_VMA_BASE)
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
