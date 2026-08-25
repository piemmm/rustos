// AP startup trampoline (Stage 3a (b)).
//
// This is the position-independent 16→64-bit startup payload the BSP
// copies to the 4 KiB-aligned low physical frame at `AP_TRAMPOLINE_PHYS`
// (`smp.rs::AP_TRAMPOLINE_PHYS`, currently `0x8000`) before sending the
// INIT-SIPI-SIPI sequence documented in Intel SDM Vol 3A §8.4.4.1.
//
// SAFETY-INVARIANTS:
//
//  1. The payload is assembled into a dedicated `.ap_trampoline` section
//     so it is loaded into the kernel image as opaque bytes. All
//     intra-payload references are resolved by the assembler as
//     `(label - _ap_trampoline_start)` constants, so the payload is
//     position-independent within its own page. The BSP-side installer
//     in `smp.rs` performs a byte-exact `copy_nonoverlapping` to physical
//     `AP_TRAMPOLINE_PHYS`.
//  2. The SIPI vector the BSP sends is `(AP_TRAMPOLINE_PHYS >> 12) & 0xFF`.
//     With `AP_TRAMPOLINE_PHYS = 0x8000` that is `0x08`, so the AP enters
//     real mode with CS = 0x0800, IP = 0x0000 and DS undefined.
//  3. The AP reads its per-CPU boot record from a fixed offset
//     (`AP_BOOT_SLOT_OFFSET`, currently `0xF00`) inside the same 4 KiB
//     frame. The BSP must populate the record AND publish-release the
//     trampoline page before sending the first SIPI.
//  4. The `ready` flag at `+0x40` inside the boot record is set to 1 by
//     the AP after long mode is established and its stack is loaded.
//     The BSP busy-waits on this flag (acquire) before launching the
//     next AP — the 4 KiB trampoline frame is shared serially.
//  5. The trampoline-internal GDT below has a 32-bit code segment at
//     selector 0x08 (used for the protected-mode trampoline step) and
//     a 64-bit code segment at selector 0x10 (L=1, used after entering
//     long mode). Both have base 0 / limit 4 GiB so they cover the
//     entire identity-mapped window the BSP set up.
//  6. The trampoline re-uses the BSP's `boot_pml4` unchanged — the BSP
//     wrote `slot.cr3` to the bootstrap PML4 phys. APs therefore see
//     exactly the same identity-mapped 0..4 GiB window the BSP sees.
//  7. On entry to `tairix_arch_x86_64_ap_main` the AP holds:
//        %rdi = slot.cpu_id (zero-extended from the 32-bit field)
//        %rsi = AP_TRAMPOLINE_PHYS + AP_BOOT_SLOT_OFFSET
//        %rsp = slot.stack_top (16-byte aligned by the BSP installer)
//     The Rust callee is `-> !`. If it ever returns the trampoline halts
//     the AP with interrupts masked (belt-and-braces).
//  8. Interrupts are disabled on the AP throughout this payload. The
//     IDTR is invalid; the Rust-side `ap_entry` must install one (or
//     keep interrupts masked) before re-enabling them.

.section .ap_trampoline, "ax"
.code16
.global _ap_trampoline_start
.global _ap_trampoline_end
.global _ap_trampoline_boot_slot_offset

// --- Layout constants resolved by the assembler ---------------------
// Offsets are computed relative to `_ap_trampoline_start` so the BSP
// installer in `smp.rs` can address fields without knowing the linked VA.

// In-page offsets, ascending:
//   [0x000 .. ~0x200]  16-bit + 64-bit code
//   [0xE00 .. 0xE20]   trampoline-internal GDT (4 × 8 bytes)
//   [0xEE0 .. 0xEE6]   GDTR record (limit + 32-bit base)
//   [0xF00 .. 0xF44]   ApBootSlot (read by the AP only; written by BSP)
//   AP_TRAMPOLINE_LEN  = AP_BOOT_SLOT_OFFSET = 0xF00, i.e. the payload
//                        proper is the part the BSP copies verbatim.
.equ AP_GDT_OFFSET,             0xE00
.equ AP_GDTR_OFFSET,            0xEE0
.equ AP_BOOT_SLOT_OFFSET,       0xF00
.equ AP_BOOT_SLOT_CR3,          0x00     // u64
.equ AP_BOOT_SLOT_STACK_TOP,    0x08     // u64
.equ AP_BOOT_SLOT_ENTRY,        0x10     // u64 — Rust callee (-> !)
.equ AP_BOOT_SLOT_CPU_ID,       0x18     // u32
.equ AP_BOOT_SLOT_READY,        0x40     // u32 (volatile; AP stores 1)

_ap_trampoline_start:

    // SIPI delivery places us in real mode with CS = vector, IP = 0.
    // Set DS = CS so memory operands with `[disp16]` resolve into our
    // trampoline page.
    cli
    cld
    movw    %cs, %ax
    movw    %ax, %ds
    movw    %ax, %es
    movw    %ax, %ss

    // Load the trampoline-internal GDT (32-bit pointer, 16-bit limit).
    // The GDTR record lives at fixed offset AP_GDTR_OFFSET inside this
    // page and was populated by the assembler below (SAFETY-INVARIANT 1).
    lgdtl   AP_GDTR_OFFSET

    // Enable PAE (CR4.PAE = 1). Required before long mode is armed.
    movl    %cr4, %eax
    orl     $(1 << 5), %eax
    movl    %eax, %cr4

    // Load CR3 with the BSP-supplied PML4. We need the 32-bit physical
    // address; the bootstrap PML4 sits below 4 GiB so the high dword of
    // slot.cr3 is zero.
    movl    AP_BOOT_SLOT_OFFSET + AP_BOOT_SLOT_CR3, %eax
    movl    %eax, %cr3

    // Arm long mode and execute-disable before paging can consume an NX leaf.
    // wrmsr requires CPL = 0; we are in real mode at CPL = 0.
    movl    $0xC0000080, %ecx
    rdmsr
    orl     $(1 << 8) | (1 << 11), %eax       // LME | NXE
    wrmsr

    // Enable PE and PG simultaneously in one MOV to CR0. Per SDM §10.8.5
    // ("Initializing IA-32e Mode") this is the supported transition out
    // of real mode straight into IA-32e compatibility mode.
    movl    %cr0, %eax
    orl     $(1 << 0) | (1 << 31), %eax        // PE | PG
    movl    %eax, %cr0

    // Far jump into the 64-bit code segment (selector 0x10) at the
    // absolute physical address of `_ap_long_mode`. AP_TRAMPOLINE_PHYS
    // = 0x8000 is encoded as the high bits below; the assembler resolves
    // `(_ap_long_mode - _ap_trampoline_start)` to a constant offset
    // within the page (SAFETY-INVARIANT 1, 5).
    ljmpl   $0x10, $(0x8000 + (_ap_long_mode - _ap_trampoline_start))

.code64
_ap_long_mode:
    // Load a sensible 64-bit data selector everywhere. Selector 0x18 in
    // the trampoline GDT is a flat data segment with base 0, limit 4 GiB
    // — adequate while we are still using the AP trampoline's own GDT.
    movw    $0x18, %ax
    movw    %ax, %ds
    movw    %ax, %es
    movw    %ax, %fs
    movw    %ax, %gs
    movw    %ax, %ss

    // Recover the boot-slot pointer in %rsi. The trampoline lives at
    // AP_TRAMPOLINE_PHYS = 0x8000; the slot sits at +AP_BOOT_SLOT_OFFSET.
    movq    $(0x8000 + AP_BOOT_SLOT_OFFSET), %rsi

    // Load the per-AP stack top. The BSP installer guarantees 16-byte
    // alignment.
    movq    AP_BOOT_SLOT_STACK_TOP(%rsi), %rsp
    xorq    %rbp, %rbp

    // Argument 1 = cpu_id (zero-extended from u32).
    movl    AP_BOOT_SLOT_CPU_ID(%rsi), %edi

    // Publish "AP is live" *before* calling the Rust entry. The BSP
    // observes this flag with an acquire-load to know it is safe to
    // launch the next AP (the trampoline page is serial-reusable per
    // SAFETY-INVARIANT 4). A release store from this CPU pairs with an
    // acquire load on the BSP.
    movl    $1, %eax
    xchgl   %eax, AP_BOOT_SLOT_READY(%rsi)

    // Indirect call to slot.entry. The Rust contract on that function
    // pointer is `extern "C" fn(cpu_id: u32) -> !` (SAFETY-INVARIANT 7).
    movq    AP_BOOT_SLOT_ENTRY(%rsi), %rax
    call    *%rax

    // `-> !` contract violated. Halt forever with interrupts masked.
.Lap_hang:
    cli
    hlt
    jmp     .Lap_hang

// --- Trampoline-internal GDT ---------------------------------------
//
// We can't put this in a separate section because the BSP copies the
// whole page verbatim. So we manually pad to AP_GDT_OFFSET and emit the
// table there.

.org AP_GDT_OFFSET
ap_gdt:
    .quad 0                                  // 0x00 — null
    .quad 0x00CF9A000000FFFF                 // 0x08 — 32-bit code (G=1, D=1, base 0, limit 0xFFFFF)
    .quad 0x00AF9A000000FFFF                 // 0x10 — 64-bit code (G=1, L=1, base 0)
    .quad 0x00CF92000000FFFF                 // 0x18 — flat data    (G=1, D=1, base 0, limit 0xFFFFF)
ap_gdt_end:

// --- GDTR record -----------------------------------------------------
//
// `lgdt` expects a 6-byte (in 32-bit operand size) memory operand:
// limit (u16) followed by base (u32). The base is the absolute physical
// address of `ap_gdt` once the page is at AP_TRAMPOLINE_PHYS = 0x8000.

.org AP_GDTR_OFFSET
ap_gdtr:
    .word ap_gdt_end - ap_gdt - 1
    .long 0x8000 + (ap_gdt - _ap_trampoline_start)

// --- Trailing pad up to the boot-slot offset is left to the BSS image.
// The installer in `smp.rs` zeroes the page before copying the payload,
// so any unwritten bytes between here and AP_BOOT_SLOT_OFFSET are zero.

// Symbol marking the end of the assembled payload. The BSP copies
// `[_ap_trampoline_start, _ap_trampoline_end)` to AP_TRAMPOLINE_PHYS.
// We deliberately keep the payload below AP_BOOT_SLOT_OFFSET so the
// boot-slot region at +0xF00 is never overwritten by the payload copy.
.org AP_BOOT_SLOT_OFFSET
_ap_trampoline_end:

// Constant the Rust side reads to confirm the assembly-time offset and
// the Rust-side mirror agree. Linked as a 1-byte symbol whose *address*
// is `AP_BOOT_SLOT_OFFSET` relative to `_ap_trampoline_start`.
_ap_trampoline_boot_slot_offset = AP_BOOT_SLOT_OFFSET
