// TAIRiX aarch64 EL1 exception vector table for the QEMU `virt` board.
//
// `VBAR_EL1` (set by `exceptions::init_vectors`) points at
// `tairix_aarch64_vectors`, which must be 2 KiB aligned (the low 11 bits
// of VBAR are RES0). The table has the architecture-mandated 16 entries
// of 0x80 bytes each (ARM ARM D1.10.2), grouped by the source:
//
//   0x000 Current EL with SP0:  Sync / IRQ / FIQ / SError
//   0x200 Current EL with SPx:  Sync / IRQ / FIQ / SError   <- kernel runs here
//   0x400 Lower EL (AArch64):   Sync / IRQ / FIQ / SError   <- EL0 syscalls/faults
//   0x600 Lower EL (AArch32):   Sync / IRQ / FIQ / SError
//
// Each entry tags the exception with a numeric *kind* and branches to a
// common trampoline that saves the interrupted GP and FP/SIMD registers, calls the
// Rust handler `tairix_aarch64_trap_handler(kind)`, restores, and
// `eret`s. The original x0/x1 are spilled *before* the kind is loaded so
// an IRQ returns with the interrupted context intact.
//
// SAFETY-INVARIANTS:
//   1. The table is 2 KiB aligned (`.balign 0x800`) and each entry is
//      0x80-aligned, so `VBAR_EL1` addresses every entry correctly.
//   2. The 816-byte frame is 16-byte aligned (SP stays 16-aligned, an
//      AArch64 requirement) and holds x0..x30 (offsets 0..240) followed by
//      the per-exception return state `ELR_EL1`/`SPSR_EL1`/`SP_EL0`
//      (offsets 248/256/264), FPCR/FPSR (offsets 272/280), q0..q31
//      (offsets 288..799), and `TPIDR_EL0` (offset 800); callee-saved
//      x19..x28/x29 are preserved by the `extern "C"` handler but saved here
//      anyway so the frame layout is uniform and the restore is symmetric.
//      The first 31 slots are the `[u64; SAVED_GPRS]` frame
//      `crate::syscall_entry` reads, so their offsets must not move.
//   3. `ELR_EL1`/`SPSR_EL1`/`SP_EL0` are saved into the frame on entry and
//      written back before `eret`, so the interrupted context resumes
//      correctly **even if a cooperative context switch ran another task
//      (which clobbers those system registers via its own trap/`eret`)
//      while this exception was suspended mid-handler** (the SP2 resumable
//      user-kthread runtime, `plans/SPAWN.md` SP2 — without this, a parked
//      `wait`/`yield` would `eret` to the wrong task's PC/stack). A handler
//      that must not return (an unhandled fault) parks the CPU instead.
//   4. Every asynchronous exception is masked for the whole return
//      sequence, from the `ELR_EL1`/`SPSR_EL1` write to the `eret`. Those
//      two registers are single-copy: an exception taken once they hold
//      this context's return state overwrites both in hardware, and the
//      nested handler's own return restores *its* saved pair, so this
//      `eret` would resume at the nested handler's return address in the
//      nested handler's PSTATE — re-entering this epilogue at EL1 with the
//      frame already popped, which walks `sp` up off the kernel stack one
//      frame per turn until the loads fault, and faults recursively with
//      `DAIF` masked (a silent, unrecoverable wedge). The debug watchdog's
//      Group-0/FIQ cadence is a live source of exactly that exception: the
//      syscall/fault handler runs with `DAIF.F` clear so a wedged core can
//      be sampled (`plans/WATCHDOG.md`). Masking here keeps the sampler out
//      of the restore tail only — never out of the handler body, which is
//      the span worth observing.
//   5. `TPIDR_EL0` — the AArch64 psABI thread pointer — is framed for the
//      same reason as `SP_EL0`, but the reason is per-*thread* rather than
//      per-exception: several threads of one process share an address space,
//      so the register is the only thing distinguishing their thread-local
//      storage. It is architecturally writable by EL0, so the kernel must not
//      hold a value of its own and overwrite a thread's own write; saving and
//      restoring it here — in a frame that lives on the interrupting thread's
//      own kernel stack — makes it per-thread and context-switch-safe by
//      construction (`plans/THREADS.md` decision 7).

.section .text
.balign 0x800
.global tairix_aarch64_vectors
tairix_aarch64_vectors:

    // --- Current EL with SP0 (unused: the kernel runs on SP_EL1) ---
    .balign 0x80                 // 0x000 Synchronous
    sub     sp, sp, #816
    stp     x0, x1, [sp]
    mov     x0, #0
    b       tairix_aarch64_trap_common

    .balign 0x80                 // 0x080 IRQ
    sub     sp, sp, #816
    stp     x0, x1, [sp]
    mov     x0, #1
    b       tairix_aarch64_trap_common

    .balign 0x80                 // 0x100 FIQ
    sub     sp, sp, #816
    stp     x0, x1, [sp]
    mov     x0, #2
    b       tairix_aarch64_trap_common

    .balign 0x80                 // 0x180 SError
    sub     sp, sp, #816
    stp     x0, x1, [sp]
    mov     x0, #3
    b       tairix_aarch64_trap_common

    // --- Current EL with SPx (the kernel's own exceptions) ---
    .balign 0x80                 // 0x200 Synchronous (fault)
    sub     sp, sp, #816
    stp     x0, x1, [sp]
    mov     x0, #4
    b       tairix_aarch64_trap_common

    .balign 0x80                 // 0x280 IRQ (timer / SGI)
    sub     sp, sp, #816
    stp     x0, x1, [sp]
    mov     x0, #5
    b       tairix_aarch64_trap_common

    .balign 0x80                 // 0x300 FIQ
    sub     sp, sp, #816
    stp     x0, x1, [sp]
    mov     x0, #6
    b       tairix_aarch64_trap_common

    .balign 0x80                 // 0x380 SError
    sub     sp, sp, #816
    stp     x0, x1, [sp]
    mov     x0, #7
    b       tairix_aarch64_trap_common

    // --- Lower EL using AArch64 (EL0 syscalls / user faults) ---
    .balign 0x80                 // 0x400 Synchronous (svc / user fault)
    sub     sp, sp, #816
    stp     x0, x1, [sp]
    mov     x0, #8
    b       tairix_aarch64_trap_common

    .balign 0x80                 // 0x480 IRQ
    sub     sp, sp, #816
    stp     x0, x1, [sp]
    mov     x0, #9
    b       tairix_aarch64_trap_common

    .balign 0x80                 // 0x500 FIQ
    sub     sp, sp, #816
    stp     x0, x1, [sp]
    mov     x0, #10
    b       tairix_aarch64_trap_common

    .balign 0x80                 // 0x580 SError
    sub     sp, sp, #816
    stp     x0, x1, [sp]
    mov     x0, #11
    b       tairix_aarch64_trap_common

    // --- Lower EL using AArch32 (unsupported on this port) ---
    .balign 0x80                 // 0x600 Synchronous
    sub     sp, sp, #816
    stp     x0, x1, [sp]
    mov     x0, #12
    b       tairix_aarch64_trap_common

    .balign 0x80                 // 0x680 IRQ
    sub     sp, sp, #816
    stp     x0, x1, [sp]
    mov     x0, #13
    b       tairix_aarch64_trap_common

    .balign 0x80                 // 0x700 FIQ
    sub     sp, sp, #816
    stp     x0, x1, [sp]
    mov     x0, #14
    b       tairix_aarch64_trap_common

    .balign 0x80                 // 0x780 SError
    sub     sp, sp, #816
    stp     x0, x1, [sp]
    mov     x0, #15
    b       tairix_aarch64_trap_common

// Common trampoline. On entry: 816-byte frame reserved, x0/x1 already
// spilled at [sp,#0], x0 = exception kind.
.balign 4
tairix_aarch64_trap_common:
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

    // Save the per-exception return state alongside the GP registers so a
    // cooperative context switch that runs another task mid-handler (the
    // SP2 resumable user-kthread runtime) cannot corrupt the resume: each
    // exception restores its own ELR_EL1/SPSR_EL1/SP_EL0 below before
    // `eret`. x2/x3 are scratch here (already spilled at [sp,#16]).
    mrs     x2, ELR_EL1
    mrs     x3, SPSR_EL1
    stp     x2, x3, [sp, #248]
    mrs     x2, SP_EL0
    str     x2, [sp, #264]

    // The psABI thread pointer, per-thread rather than per-exception
    // (invariant 5): several threads of one process share one address space,
    // so this register is what makes their thread-local storage distinct.
    mrs     x2, TPIDR_EL0
    str     x2, [sp, #800]

    // Preserve the complete interrupted FP/SIMD state. An IRQ can suspend
    // this handler while another task runs and uses arbitrary vector
    // registers, so preserving only the AAPCS64 callee-saved subset would
    // corrupt user state that never crossed a function-call boundary.
    stp     q0, q1, [sp, #288]
    stp     q2, q3, [sp, #320]
    stp     q4, q5, [sp, #352]
    stp     q6, q7, [sp, #384]
    stp     q8, q9, [sp, #416]
    stp     q10, q11, [sp, #448]
    stp     q12, q13, [sp, #480]
    stp     q14, q15, [sp, #512]
    stp     q16, q17, [sp, #544]
    stp     q18, q19, [sp, #576]
    stp     q20, q21, [sp, #608]
    stp     q22, q23, [sp, #640]
    stp     q24, q25, [sp, #672]
    stp     q26, q27, [sp, #704]
    stp     q28, q29, [sp, #736]
    stp     q30, q31, [sp, #768]
    mrs     x2, FPCR
    mrs     x3, FPSR
    stp     x2, x3, [sp, #272]

    // x0 still holds the exception kind; pass the saved-frame base in x1
    // so the handler can read the EL0 syscall registers (x0..x8 at
    // [sp,#0..#64]) and write the syscall result back into the x0 slot
    // before the symmetric restore + `eret`.
    mov     x1, sp
    bl      tairix_aarch64_trap_handler

    // Close the return sequence to asynchronous exceptions before the
    // return state goes into ELR_EL1/SPSR_EL1: an FIQ, IRQ or SError taken
    // between that write and the `eret` below overwrites both registers,
    // and the nested handler restores its own pair, destroying this
    // context's resume irrecoverably (invariant 4 above). `eret` reloads
    // PSTATE from SPSR_EL1, so the mask does not reach the resumed context.
    msr     DAIFSet, #0xf

    // Restore the per-exception return state (using x2/x3 as scratch before
    // they are reloaded from their GP slots below) so `eret` returns to the
    // interrupted PC/PSTATE on the interrupted EL0 stack, regardless of any
    // intervening context switch.
    ldp     x2, x3, [sp, #248]
    msr     ELR_EL1, x2
    msr     SPSR_EL1, x3
    ldr     x2, [sp, #264]
    msr     SP_EL0, x2
    ldr     x2, [sp, #800]
    msr     TPIDR_EL0, x2

    ldp     x2, x3, [sp, #272]
    msr     FPCR, x2
    msr     FPSR, x3
    ldp     q30, q31, [sp, #768]
    ldp     q28, q29, [sp, #736]
    ldp     q26, q27, [sp, #704]
    ldp     q24, q25, [sp, #672]
    ldp     q22, q23, [sp, #640]
    ldp     q20, q21, [sp, #608]
    ldp     q18, q19, [sp, #576]
    ldp     q16, q17, [sp, #544]
    ldp     q14, q15, [sp, #512]
    ldp     q12, q13, [sp, #480]
    ldp     q10, q11, [sp, #448]
    ldp     q8, q9, [sp, #416]
    ldp     q6, q7, [sp, #384]
    ldp     q4, q5, [sp, #352]
    ldp     q2, q3, [sp, #320]
    ldp     q0, q1, [sp, #288]

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
    add     sp, sp, #816
    eret
