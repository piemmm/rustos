// Multiboot2 entry + 32→64 long-mode trampoline.
//
// We use multiboot2 (not multiboot1) because QEMU's `-kernel` multiboot1
// loader refuses to load ELF64, whereas GRUB's multiboot2 loader accepts
// ELF64 and enters the kernel in 32-bit protected mode at the e_entry
// symbol. The kernel is therefore booted via `grub-mkrescue` ISO and
// QEMU `-cdrom`, not `-kernel`.
//
// SAFETY-INVARIANTS (audited per AGENTS.md §10):
//
//  1. The multiboot2 header sits in `.multiboot_header`, placed by the
//     linker script (`linker.ld`) within the first 32 KiB of the kernel
//     ELF — required by the multiboot2 spec for the loader to find it.
//  2. `_start` runs in 32-bit protected mode with CR0.PE=1, paging off,
//     EAX=multiboot2 magic (0x36D76289), EBX=multiboot info pointer,
//     and a flat 4 GiB code/data segmentation set up by GRUB.
//  3. The 4 KiB-aligned `boot_pml4`/`boot_pdpt`/`boot_pdpt_high`/`boot_pds`
//     tables sit in `.boot.bss` (linked 1:1 in low memory by `linker.ld`)
//     and are zero-initialised by the multiboot loader (BSS bytes are zero
//     per the multiboot spec). They live in `.boot.bss` rather than the
//     high-half `.bss` so the 32-bit trampoline can name them with absolute
//     32-bit operands before paging is enabled.
//  4. We identity-map the full 0..4 GiB physical address window via four
//     page directories chained from PDPT[0..4], each populated with the
//     512 2 MiB huge-page entries that cover its 1 GiB slot. This is
//     deliberately broader than the original 32 MiB bootstrap map: it
//     guarantees that the LAPIC MMIO frame at 0xFEE00000 and the IO-APIC
//     frame at 0xFEC00000 — both architecturally fixed by Intel — are
//     reachable, and likewise that any ACPI table OVMF/GRUB placed in
//     high memory below 4 GiB is reachable. Going beyond 4 GiB is not
//     needed for the Stage-2 QEMU tests (the runner allocates 256 MiB of
//     guest RAM and no MMIO RustOS uses today sits above 4 GiB).
//  5. The long-mode GDT below has a single 64-bit code segment at
//     selector 0x08 with L=1 (long mode) and a 64-bit data segment at
//     selector 0x10. Both are flat (base 0, limit ignored in 64-bit).
//  6. On entry to `long_mode_start` interrupts are disabled (CLI is the
//     bootloader default) and the IDTR is invalid; the Rust side must
//     install an IDT before enabling interrupts (`AGENTS.md` §10).
//  7. `rustos_arch_x86_64_main` receives the multiboot magic in `%rdi`
//     and the multiboot info pointer in `%rsi` (System V AMD64 ABI).
//     The Rust prologue re-validates the magic before touching the info
//     blob (`AGENTS.md` §5.4.3 — validate every input).
//  8. If `rustos_arch_x86_64_main` ever returns we halt the CPU with
//     interrupts masked. The Rust contract (`-> !`) makes this branch
//     unreachable; the `hlt`/`jmp` loop is the belt-and-braces fallback
//     `AGENTS.md` §2.9 requires.
//  9. The kernel is a -2 GiB higher-half kernel (`linker.ld`): its Rust
//     sections are linked at virtual `KERNEL_VMA_BASE (0xFFFFFFFF80000000)
//     + phys` but loaded into low physical memory. Before the long-mode
//     jump the trampoline maps that window (PML4[511] -> `boot_pdpt_high`,
//     `boot_pdpt_high`[510] -> the first-GiB identity PD `boot_pds`), so
//     virtual `0xFFFFFFFF80000000 + X` resolves to physical `X` for the
//     whole kernel image. The 0..4 GiB identity map (invariant 4) is kept
//     so the direct physical map (`kernel/mem` phys.rs: DMA/MMIO/ACPI/
//     multiboot info) is unaffected. After entering long mode the
//     trampoline transfers to the high half with an absolute
//     `movabs`+`jmp *%rax` to `higher_half_entry`.

.section .multiboot_header, "a"
.align 8
multiboot_header_start:
    // Multiboot2 header (multiboot2 spec §3.1):
    .long 0xE85250D6                                // magic
    .long 0                                         // architecture: i386 32-bit protected mode
    .long multiboot_header_end - multiboot_header_start            // header_length
    // Checksum: -(magic + architecture + header_length), mod 2^32
    .long -(0xE85250D6 + 0 + (multiboot_header_end - multiboot_header_start))

    // End tag (type=0, flags=0, size=8). Required terminator.
    .align 8
    .short 0
    .short 0
    .long 8
multiboot_header_end:

.section .boot.text, "ax"
.code32
.global _start
.type _start, @function
_start:
    cli
    movl %eax, %edi                                 // multiboot magic (preserved via %edi -> %rdi)
    movl %ebx, %esi                                 // multiboot info pointer

    movl $boot_stack_top, %esp
    xorl %ebp, %ebp

    // PML4[0] -> PDPT
    movl $boot_pdpt, %eax
    orl  $0x3, %eax                                 // P|RW
    movl %eax, boot_pml4

    // PDPT[i] -> boot_pds + i*4096 | P|RW, for i in 0..4  (one PD per GiB).
    xorl %ecx, %ecx
1:
    movl $boot_pds, %eax
    movl %ecx, %edx
    shll $12, %edx                                  // ecx * 4096
    addl %edx, %eax
    orl  $0x3, %eax                                 // P|RW
    movl %eax, boot_pdpt(,%ecx,8)
    movl $0, boot_pdpt+4(,%ecx,8)
    incl %ecx
    cmpl $4, %ecx
    jl   1b

    // PDS[k] = k*2 MiB | P|RW|PS, for k in 0..2048
    // (identity-map the full 0..4 GiB window; high 32 bits of the PDE
    //  are always zero because k * 2 MiB < 2^32).
    xorl %ecx, %ecx
2:
    movl %ecx, %eax
    shll $21, %eax                                  // ecx * 2 MiB (low 32 bits)
    orl  $0x83, %eax                                // P|RW|PS
    movl %eax, boot_pds(,%ecx,8)
    movl $0, boot_pds+4(,%ecx,8)
    incl %ecx
    cmpl $2048, %ecx
    jl   2b

    // Higher-half kernel window (SAFETY-INVARIANT 9). The kernel's Rust
    // sections are linked at KERNEL_VMA_BASE = 0xFFFFFFFF80000000 + phys
    // (linker.ld) but loaded into low physical memory. 0xFFFFFFFF80000000
    // decodes to PML4[511] -> PDPT[510] -> PD[0]; pointing PDPT[510] at the
    // already-populated first-GiB identity PD (`boot_pds`) maps virtual
    // 0xFFFFFFFF80000000 + X to physical X for X in 0..1 GiB, which is
    // exactly the kernel image's load window. The 0..4 GiB identity map
    // above is kept intact so the direct physical map (DMA/MMIO, ACPI,
    // multiboot info, LAPIC/IO-APIC MMIO) keeps working unchanged.
    //
    // PML4[511] -> boot_pdpt_high  (offset 511 * 8 = 0xFF8)
    movl $boot_pdpt_high, %eax
    orl  $0x3, %eax                                 // P|RW
    movl %eax, boot_pml4 + 0xFF8
    movl $0, boot_pml4 + 0xFFC

    // boot_pdpt_high[510] -> boot_pds  (offset 510 * 8 = 0xFF0)
    movl $boot_pds, %eax
    orl  $0x3, %eax                                 // P|RW
    movl %eax, boot_pdpt_high + 0xFF0
    movl $0, boot_pdpt_high + 0xFF4

    // CR3 <- PML4
    movl $boot_pml4, %eax
    movl %eax, %cr3

    // CR4.PAE = 1
    movl %cr4, %eax
    orl  $(1 << 5), %eax
    movl %eax, %cr4

    // EFER.LME = 1
    movl $0xC0000080, %ecx
    rdmsr
    orl  $(1 << 8), %eax
    wrmsr

    // CR0.PG = 1 (paging on); we're now in compatibility mode.
    movl %cr0, %eax
    orl  $(1 << 31), %eax
    movl %eax, %cr0

    // Load 64-bit GDT, far-jump to 64-bit code segment.
    lgdt gdt64_ptr
    ljmp $0x08, $long_mode_start

.size _start, . - _start

.code64
long_mode_start:
    // Long mode ignores data segment bases/limits but the selectors must
    // be non-NULL writable; load our flat data selector everywhere.
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %fs
    movw %ax, %gs
    movw %ax, %ss

    // We are still executing at the low (identity-mapped) physical RIP of
    // `.boot.text`. The higher-half window is now mapped, so transfer to
    // the kernel's high virtual address space with an absolute jump: the
    // `movabs` materialises the full 64-bit link address of the high-half
    // landing pad (an R_X86_64_64 relocation the linker fills with its
    // KERNEL_VMA_BASE-relative VA), and `jmp *%rax` loads RIP with it.
    // rdi/rsi (multiboot magic / info pointer) are preserved — only %rax
    // is clobbered.
    movabs $higher_half_entry, %rax
    jmp *%rax

.size long_mode_start, . - long_mode_start

// -- Higher-half landing pad. Linked into `.text` at KERNEL_VMA_BASE + phys
//    (linker.ld), so reaching here means RIP is running in the higher-half
//    kernel window. From here a normal RIP-relative `call` reaches the Rust
//    entry point (both are high-half symbols). The boot stack stays valid
//    because the 0..4 GiB identity map is preserved.
.section .text, "ax"
.code64
.global higher_half_entry
.type higher_half_entry, @function
higher_half_entry:
    // rdi/rsi still hold the multiboot magic / info pointer (untouched by
    // the absolute jump above).
    call rustos_arch_x86_64_main

    // `rustos_arch_x86_64_main` is `-> !`; reaching here is a kernel bug.
    cli
.Lhang:
    hlt
    jmp .Lhang

.size higher_half_entry, . - higher_half_entry

// -- BSS-allocated bootstrap page tables and stack. The multiboot2 spec
//    guarantees BSS is zero-initialised before `_start` runs. These live in
//    the low `.boot.bss` section (linker.ld) so the 32-bit trampoline can
//    name them with absolute 32-bit operands before paging is enabled.
.section .boot.bss, "aw", @nobits
.align 4096
.global boot_pml4
boot_pml4:
    .skip 4096
boot_pdpt:
    .skip 4096
// PDPT for the higher-half kernel window (SAFETY-INVARIANT 9). Its entry
// 510 points at `boot_pds` so 0xFFFFFFFF80000000 + X maps to physical X.
boot_pdpt_high:
    .skip 4096
// Four contiguous PDs, one per GiB of the identity-mapped 0..4 GiB window.
// See SAFETY-INVARIANT 4. Symbol exposed so the AP bring-up code in
// `smp.rs` can pass the bootstrap PML4 to APs unchanged. The first-GiB PD
// is reused by `boot_pdpt_high` to back the higher-half kernel window.
.global boot_pds
boot_pds:
    .skip 4096 * 4

// The bootstrap stack the BSP runs on for the whole pre-handoff boot
// path and, in the QEMU integration verticals, for the device-bring-up
// scenario the audit observer drives synchronously (a real driver +
// filesystem mounted in the boot thread). That scenario nests the
// virtio bring-up, a signed-`.rxe` load/reload, and a full filesystem
// `open` (which stages whole blocks through on-stack scratch buffers)
// onto this stack, so 16 KiB was marginal; 64 KiB gives ample headroom
// and keeps an overflow from silently corrupting the adjacent
// `boot_pds` page tables. `KERNEL_STACK_BYTES` in `rustos-kernel`
// tracks this value (its static assert pins the lower bound).
.align 16
boot_stack_bottom:
    .skip 65536
boot_stack_top:

// -- Long-mode GDT. Lives in the low `.boot.rodata` section (linker.ld) so
//    `lgdt` can load it (by its low linear address) before paging is on.
.section .boot.rodata, "a"
.align 8
gdt64:
    .quad 0                                         // 0x00: null
    .quad 0x00AF9A000000FFFF                        // 0x08: 64-bit code (L=1)
    .quad 0x00AF92000000FFFF                        // 0x10: data
gdt64_end:
gdt64_ptr:
    .word gdt64_end - gdt64 - 1
    .quad gdt64
