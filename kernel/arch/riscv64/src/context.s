# riscv64 context-switch primitive.
#
# extern "C" fn tairix_arch_riscv64_switch(prev: *mut TaskCtx,
#                                           next: *mut TaskCtx);
#
# Calling convention: RISC-V integer ABI. a0 = prev, a1 = next.
#
# SAFETY-INVARIANTS (audited per AGENTS.md §10):
#
#   1. Called with `prev` and `next` non-null. The Rust-side safe
#      wrapper `crate::context::switch` documents this contract;
#      violating it is undefined behaviour by design.
#   2. `TaskCtx` has `repr(C)` layout pinned by a const-assert in
#      `context.rs` to a single `sp: u64` field at offset 0. The
#      `0(a0)` / `0(a1)`
#      operands below address that field.
#   3. The frame this routine produces on suspend / consumes on resume
#      matches `TaskCtx::prepare` byte-for-byte; the host test
#      `prepare_writes_initial_frame` is the canonical cross-check.
#   4. Only the callee-saved registers (`ra`, `s0`..`s11`) plus the
#      first argument register `a0` are saved; all other registers are
#      caller-saved per the RISC-V ABI and carry no live state across
#      the call boundary.
#   5. Interrupts may be enabled; this routine makes no atomic guarantee
#      about interrupt *delivery* across the switch. A caller needing an
#      uninterruptible switch masks `sstatus.SIE` around the call.

.section .text
.balign 4
.global tairix_arch_riscv64_switch
.type   tairix_arch_riscv64_switch, @function

tairix_arch_riscv64_switch:
    # --- Suspend half ---
    # Reserve a 112-byte frame and save ra, s0..s11, a0 in ascending
    # address order so the resume half restores them by the same offsets.
    addi    sp, sp, -112
    sd      ra, 0(sp)
    sd      s0, 8(sp)
    sd      s1, 16(sp)
    sd      s2, 24(sp)
    sd      s3, 32(sp)
    sd      s4, 40(sp)
    sd      s5, 48(sp)
    sd      s6, 56(sp)
    sd      s7, 64(sp)
    sd      s8, 72(sp)
    sd      s9, 80(sp)
    sd      s10, 88(sp)
    sd      s11, 96(sp)
    # Save a0 (the outbound `prev` pointer at entry). For a freshly
    # prepared task this slot instead holds the first-run argument; the
    # resume half loads it into a0 either way (see `TaskCtx::prepare`).
    sd      a0, 104(sp)

    # Record outgoing sp into prev.sp. a0 still holds `prev`.
    sd      sp, 0(a0)

    # --- Resume half ---
    # Load the inbound task's saved stack pointer from next.sp.
    ld      sp, 0(a1)

    ld      ra, 0(sp)
    ld      s0, 8(sp)
    ld      s1, 16(sp)
    ld      s2, 24(sp)
    ld      s3, 32(sp)
    ld      s4, 40(sp)
    ld      s5, 48(sp)
    ld      s6, 56(sp)
    ld      s7, 64(sp)
    ld      s8, 72(sp)
    ld      s9, 80(sp)
    ld      s10, 88(sp)
    ld      s11, 96(sp)
    # Restore a0: the inbound task's first-run argument, or the saved a0
    # from a prior suspend.
    ld      a0, 104(sp)
    addi    sp, sp, 112

    # `ret` jumps to `ra` — a synthesised `entry` (first run) or the
    # address after the inbound task's suspend-time call site.
    ret

.size tairix_arch_riscv64_switch, . - tairix_arch_riscv64_switch
