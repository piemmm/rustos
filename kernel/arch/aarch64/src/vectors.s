// RustOS aarch64 EL1 exception vector table for the QEMU `virt` board.
//
// `VBAR_EL1` (set by `exceptions::init_vectors`) points at
// `rustos_aarch64_vectors`, which must be 2 KiB aligned (the low 11 bits
// of VBAR are RES0). The table has the architecture-mandated 16 entries
// of 0x80 bytes each (ARM ARM D1.10.2), grouped by the source:
//
//   0x000 Current EL with SP0:  Sync / IRQ / FIQ / SError
//   0x200 Current EL with SPx:  Sync / IRQ / FIQ / SError   <- kernel runs here
//   0x400 Lower EL (AArch64):   Sync / IRQ / FIQ / SError   <- EL0 syscalls/faults
//   0x600 Lower EL (AArch32):   Sync / IRQ / FIQ / SError
//
// Each entry tags the exception with a numeric *kind* and branches to a
// common trampoline that saves the interrupted GP registers, calls the
// Rust handler `rustos_aarch64_trap_handler(kind)`, restores, and
// `eret`s. The original x0/x1 are spilled *before* the kind is loaded so
// an IRQ returns with the interrupted context intact.
//
// SAFETY-INVARIANTs (audited per AGENTS.md §10):
//   1. The table is 2 KiB aligned (`.balign 0x800`) and each entry is
//      0x80-aligned, so `VBAR_EL1` addresses every entry correctly.
//   2. The 256-byte frame is 16-byte aligned (SP stays 16-aligned, an
//      AArch64 requirement) and holds x0..x30; callee-saved x19..x28/x29
//      are preserved by the `extern "C"` handler but saved here anyway so
//      the frame layout is uniform and the restore is symmetric.
//   3. `ELR_EL1` / `SPSR_EL1` are not touched by the C handler, so `eret`
//      resumes the interrupted context. A handler that must not return
//      (an unhandled fault) parks the CPU instead of returning.

.section .text
.balign 0x800
.global rustos_aarch64_vectors
rustos_aarch64_vectors:

    // --- Current EL with SP0 (unused: the kernel runs on SP_EL1) ---
    .balign 0x80                 // 0x000 Synchronous
    sub     sp, sp, #256
    stp     x0, x1, [sp]
    mov     x0, #0
    b       rustos_aarch64_trap_common

    .balign 0x80                 // 0x080 IRQ
    sub     sp, sp, #256
    stp     x0, x1, [sp]
    mov     x0, #1
    b       rustos_aarch64_trap_common

    .balign 0x80                 // 0x100 FIQ
    sub     sp, sp, #256
    stp     x0, x1, [sp]
    mov     x0, #2
    b       rustos_aarch64_trap_common

    .balign 0x80                 // 0x180 SError
    sub     sp, sp, #256
    stp     x0, x1, [sp]
    mov     x0, #3
    b       rustos_aarch64_trap_common

    // --- Current EL with SPx (the kernel's own exceptions) ---
    .balign 0x80                 // 0x200 Synchronous (fault)
    sub     sp, sp, #256
    stp     x0, x1, [sp]
    mov     x0, #4
    b       rustos_aarch64_trap_common

    .balign 0x80                 // 0x280 IRQ (timer / SGI)
    sub     sp, sp, #256
    stp     x0, x1, [sp]
    mov     x0, #5
    b       rustos_aarch64_trap_common

    .balign 0x80                 // 0x300 FIQ
    sub     sp, sp, #256
    stp     x0, x1, [sp]
    mov     x0, #6
    b       rustos_aarch64_trap_common

    .balign 0x80                 // 0x380 SError
    sub     sp, sp, #256
    stp     x0, x1, [sp]
    mov     x0, #7
    b       rustos_aarch64_trap_common

    // --- Lower EL using AArch64 (EL0 syscalls / user faults) ---
    .balign 0x80                 // 0x400 Synchronous (svc / user fault)
    sub     sp, sp, #256
    stp     x0, x1, [sp]
    mov     x0, #8
    b       rustos_aarch64_trap_common

    .balign 0x80                 // 0x480 IRQ
    sub     sp, sp, #256
    stp     x0, x1, [sp]
    mov     x0, #9
    b       rustos_aarch64_trap_common

    .balign 0x80                 // 0x500 FIQ
    sub     sp, sp, #256
    stp     x0, x1, [sp]
    mov     x0, #10
    b       rustos_aarch64_trap_common

    .balign 0x80                 // 0x580 SError
    sub     sp, sp, #256
    stp     x0, x1, [sp]
    mov     x0, #11
    b       rustos_aarch64_trap_common

    // --- Lower EL using AArch32 (unsupported on this port) ---
    .balign 0x80                 // 0x600 Synchronous
    sub     sp, sp, #256
    stp     x0, x1, [sp]
    mov     x0, #12
    b       rustos_aarch64_trap_common

    .balign 0x80                 // 0x680 IRQ
    sub     sp, sp, #256
    stp     x0, x1, [sp]
    mov     x0, #13
    b       rustos_aarch64_trap_common

    .balign 0x80                 // 0x700 FIQ
    sub     sp, sp, #256
    stp     x0, x1, [sp]
    mov     x0, #14
    b       rustos_aarch64_trap_common

    .balign 0x80                 // 0x780 SError
    sub     sp, sp, #256
    stp     x0, x1, [sp]
    mov     x0, #15
    b       rustos_aarch64_trap_common

// Common trampoline. On entry: 256-byte frame reserved, x0/x1 already
// spilled at [sp,#0], x0 = exception kind.
.balign 4
rustos_aarch64_trap_common:
    stp     x2, x3, [sp, #16]
    stp     x4, x5, [sp, #32]
    stp     x6, x7, [sp, #48]
    stp     x8, x9, [sp, #64]
    stp     x10, x11, [sp, #80]
    stp     x12, x13, [sp, #96]
    stp     x14, x15, [sp, #112]
    stp     x16, x17, [sp, #128]
    stp     x18, x19, [sp, #144]
    stp     x20, x21, [sp, #160]
    stp     x22, x23, [sp, #176]
    stp     x24, x25, [sp, #192]
    stp     x26, x27, [sp, #208]
    stp     x28, x29, [sp, #224]
    str     x30, [sp, #240]

    // x0 still holds the exception kind; pass the saved-frame base in x1
    // so the handler can read the EL0 syscall registers (x0..x8 at
    // [sp,#0..#64]) and write the syscall result back into the x0 slot
    // before the symmetric restore + `eret`.
    mov     x1, sp
    bl      rustos_aarch64_trap_handler

    ldr     x30, [sp, #240]
    ldp     x28, x29, [sp, #224]
    ldp     x26, x27, [sp, #208]
    ldp     x24, x25, [sp, #192]
    ldp     x22, x23, [sp, #176]
    ldp     x20, x21, [sp, #160]
    ldp     x18, x19, [sp, #144]
    ldp     x16, x17, [sp, #128]
    ldp     x14, x15, [sp, #112]
    ldp     x12, x13, [sp, #96]
    ldp     x10, x11, [sp, #80]
    ldp     x8, x9, [sp, #64]
    ldp     x6, x7, [sp, #48]
    ldp     x4, x5, [sp, #32]
    ldp     x2, x3, [sp, #16]
    ldp     x0, x1, [sp]
    add     sp, sp, #256
    eret
