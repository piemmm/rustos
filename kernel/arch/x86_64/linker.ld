/* Linker script for TAIRiX x86_64 kernels (higher-half), bootable via
 * multiboot2 (GRUB) and PVH direct boot (QEMU `-kernel`).
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
 *   - The PVH boot note (`.note.tairix_pvh`, boot.s) is placed explicitly
 *     *before* the /DISCARD/ rule that drops every other note section, so
 *     the linker keeps it and emits the PT_NOTE segment QEMU's `-kernel`
 *     ELF loader scans for `XEN_ELFNOTE_PHYS32_ENTRY`.
 *   - `.boot.text` (the 32-bit trampoline) follows the header so the entry
 *     point lives at the lowest loadable address, and is linked 1:1.
 *   - The boot page tables, boot stack, and early GDT live in `.boot.bss` /
 *     `.boot.rodata`, also linked 1:1, so the 32-bit trampoline can name
 *     them with absolute 32-bit operands before paging is on. The stack is
 *     reserved here, not in `boot.s`, so an image can size it.
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

/* Boot-stack size, sized by the image rather than by this shared script:
 * 64 KiB is the boot pipeline's own need — including the device bring-up the
 * audit observer drives synchronously in the QEMU verticals, a filesystem
 * `open` staging whole blocks on the stack. An image whose workload needs
 * more links a script of its own that sets `BOOT_STACK_BYTES` and then
 * `INCLUDE`s this one, so the value is defined before this line reads it. */
BOOT_STACK_BYTES = DEFINED(BOOT_STACK_BYTES) ? BOOT_STACK_BYTES : 64K;

/* The poison guard reserved directly below the boot stack. */
BOOT_STACK_GUARD_BYTES = 4K;

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

    /* PVH boot note. Input-section matching is first-match, so placing it
     * here exempts it from the `*(.note .note.*)` /DISCARD/ below. */
    .note.tairix_pvh ALIGN(4) :
    {
        KEEP(*(.note.tairix_pvh))
    }

    .boot.bss ALIGN(4K) :
    {
        *(.boot.bss)
        /* The bootstrap stack the BSP runs on for the whole pre-handoff
         * boot path, directly above the page tables `boot.s` reserves.
         * Paging is off when the BSP first descends it, so an overrun
         * cannot be caught by unmapping a page: `boot_common` fills the
         * guard below it with the shared sentinel and the panic path
         * reads it back, turning a silent clobber of `boot_pds` into a
         * named overrun. The guard is every byte up to
         * `boot_stack_bottom`, padding included, so the fill and the
         * check share one pair of symbols. */
        . = ALIGN(4K);
        boot_stack_guard_bottom = .;
        . += BOOT_STACK_GUARD_BYTES;
        . = ALIGN(16);
        boot_stack_bottom = .;
        . += BOOT_STACK_BYTES;
        boot_stack_top = .;
        /* The boot CPU's descriptor tables, which `boot.s` hands to the
         * entry: sized by the `boot_tables_bytes` symbol the port defines
         * from the type itself. */
        . = ALIGN(16);
        boot_tables = .;
        . += boot_tables_bytes;
    }

    . = ALIGN(4K);
    __boot_phys_end = .;

    /* The boot stack's higher-half aliases: the same bytes, addressed
     * through the kernel window, so `%rsp` is canonical from the moment
     * the trampoline lands in the higher half and stays mapped under a
     * process root, which carries no identity map. Computed here because
     * this script is the one definition of KERNEL_VMA_BASE. */
    boot_stack_bottom_high = boot_stack_bottom + KERNEL_VMA_BASE;
    boot_stack_top_high = boot_stack_top + KERNEL_VMA_BASE;
    /* The stack's poison guard, aliased through the same window: the panic
     * path reads it long after the trampoline has left the identity map
     * behind. */
    boot_stack_guard_bottom_high = boot_stack_guard_bottom + KERNEL_VMA_BASE;
    boot_tables_high = boot_tables + KERNEL_VMA_BASE;

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

    /* One-past-the-end *physical* address of the whole kernel image
     * (boot trampoline through the end of .bss, which includes the bin's
     * bump heap). The boot path reserves [__boot_phys_start,
     * __kernel_phys_end) out of the firmware-usable RAM so the frame
     * allocator never hands out a frame overlapping the running kernel or
     * its heap (the higher-half VA `.` maps to physical `. - KERNEL_VMA_BASE`,
     * the LMA every section above is loaded at). */
    . = ALIGN(4K);
    __kernel_phys_end = . - KERNEL_VMA_BASE;

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
