// aarch64 context-switch primitive.
//
// extern "C" fn tairix_arch_aarch64_switch(prev: *mut TaskCtx,
//                                            next: *mut TaskCtx);
//
// Calling convention: AAPCS64. x0 = prev, x1 = next.
//
// SAFETY-INVARIANTS:
//
//   1. Called with `prev` and `next` non-null. The Rust-side safe
//      wrapper `crate::context::switch` documents this contract;
//      violating it is undefined behaviour by design.
//   2. `TaskCtx` has `repr(C)` layout pinned by a const-assert in
//      `context.rs` to a single `sp: u64` field at offset 0. The
//      `[x0]` / `[x1]` operands below address that field.
//   3. The frame this routine produces on suspend / consumes on resume
//      matches `TaskCtx::prepare` byte-for-byte; the host test
//      `prepare_writes_initial_frame` is the canonical cross-check.
//   4. The AAPCS64 callee-saved registers (x19..x28, x29/FP, x30/LR,
//      and d8..d15) plus the first argument register x0 are saved; all
//      other registers are caller-saved and carry no live state across
//      the call boundary. The exception trampoline separately preserves
//      complete q0..q31 user state across an involuntary switch.
//   5. DAIF is saved with each suspended continuation and restored only
//      after the inbound stack and registers are complete. The all-ones
//      marker in a synthesised first-run frame means inherit the dispatcher's
//      current DAIF; every subsequent frame contains an exact saved value.

.section .text
.balign 4
.global tairix_arch_aarch64_switch
.type   tairix_arch_aarch64_switch, %function

tairix_arch_aarch64_switch:
    // --- Suspend half ---
    // Reserve a 192-byte frame and save x19..x30, x0, d8..d15, and DAIF in
    // ascending address order so the resume half restores matching offsets.
    sub     sp, sp, #192
    stp     x19, x20, [sp, #0]
    stp     x21, x22, [sp, #16]
    stp     x23, x24, [sp, #32]
    stp     x25, x26, [sp, #48]
    stp     x27, x28, [sp, #64]
    stp     x29, x30, [sp, #80]
    // Save x0 (the outbound `prev` pointer at entry). For a freshly
    // prepared task this slot instead holds the first-run argument; the
    // resume half loads it into x0 either way (see `TaskCtx::prepare`).
    str     x0, [sp, #96]
    stp     d8, d9, [sp, #112]
    stp     d10, d11, [sp, #128]
    stp     d12, d13, [sp, #144]
    stp     d14, d15, [sp, #160]
    mrs     x9, DAIF
    str     x9, [sp, #176]

    // Record outgoing sp into prev.sp. x0 still holds `prev`.
    mov     x9, sp
    str     x9, [x0]

    // --- Resume half ---
    // Load the inbound task's saved stack pointer from next.sp.
    ldr     x9, [x1]
    mov     sp, x9

    ldp     x19, x20, [sp, #0]
    ldp     x21, x22, [sp, #16]
    ldp     x23, x24, [sp, #32]
    ldp     x25, x26, [sp, #48]
    ldp     x27, x28, [sp, #64]
    ldp     x29, x30, [sp, #80]
    // Restore x0: the inbound task's first-run argument, or the saved x0
    // from a prior suspend.
    ldr     x0, [sp, #96]
    ldp     d8, d9, [sp, #112]
    ldp     d10, d11, [sp, #128]
    ldp     d12, d13, [sp, #144]
    ldp     d14, d15, [sp, #160]
    // Restore the inbound continuation's interrupt mask last. A fresh
    // frame carries UINT64_MAX and inherits the dispatcher's current DAIF.
    ldr     x9, [sp, #176]
    add     sp, sp, #192
    cmn     x9, #1
    b.eq    1f
    msr     DAIF, x9
    isb
1:

    // `ret` jumps to x30 — a synthesised `entry` (first run) or the
    // address after the inbound task's suspend-time call site.
    ret

.size tairix_arch_aarch64_switch, . - tairix_arch_aarch64_switch
