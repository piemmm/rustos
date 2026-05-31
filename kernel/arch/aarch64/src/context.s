// aarch64 context-switch primitive.
//
// extern "C" fn rustos_arch_aarch64_switch(prev: *mut TaskCtx,
//                                            next: *mut TaskCtx);
//
// Calling convention: AAPCS64. x0 = prev, x1 = next.
//
// SAFETY-INVARIANTS (audited per AGENTS.md §10):
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
//   4. Only the AAPCS64 callee-saved registers (x19..x28, x29/FP,
//      x30/LR) plus the first argument register x0 are saved; all other
//      registers are caller-saved and carry no live state across the
//      call boundary.
//   5. Interrupts may be enabled; this routine makes no atomic guarantee
//      about interrupt *delivery* across the switch. A caller needing an
//      uninterruptible switch masks `DAIF` around the call.

.section .text
.balign 4
.global rustos_arch_aarch64_switch
.type   rustos_arch_aarch64_switch, %function

rustos_arch_aarch64_switch:
    // --- Suspend half ---
    // Reserve a 112-byte frame and save x19..x30 and x0 in ascending
    // address order so the resume half restores them by the same offsets.
    sub     sp, sp, #112
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
    add     sp, sp, #112

    // `ret` jumps to x30 — a synthesised `entry` (first run) or the
    // address after the inbound task's suspend-time call site.
    ret

.size rustos_arch_aarch64_switch, . - rustos_arch_aarch64_switch
