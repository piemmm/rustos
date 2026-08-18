# TAIRiX riscv64 S-mode trap vector for the QEMU `virt` board.
#
# Installed into `stvec` (direct mode) by `trap::init_traps`. Every
# S-mode trap — synchronous exception or interrupt — enters here with
# `sstatus.SIE` cleared by hardware. The vector swaps to a kernel stack,
# saves the interrupted context's caller-saved integer registers plus the
# return-state CSRs (`sepc`, `sstatus`, and the interrupted `sp`), calls
# the Rust handler (`tairix_riscv64_trap_handler`, which preserves
# callee-saved registers per the C ABI and may advance the saved `sepc`),
# restores, and returns with `sret`.
#
# # Per-task trap anchor via `sscratch`
#
# A trap taken from U-mode must NOT run the handler on the interrupted
# user `sp`: a cooperative `ContextSwitch::switch` taken mid-handler
# (a parking `yield`/`wait`) would otherwise persist the *user* stack
# pointer as the task's saved kernel context. So the vector swaps `sp`
# with `sscratch` on entry. The invariant the rest of the port upholds is:
#
#   * while running U-mode code, `sscratch` holds the address of the
#     running task's **trap anchor**: a TRAP_ANCHOR_BYTES-wide, kernel-only
#     region at the top of the task's kernel-stack window whose first word
#     (OFF_ANCHOR_KTP) is the kernel `tp` of the hart the task is running
#     on. The trap frame is built immediately below it. Armed by
#     `userentry::enter_user` before the first `sret`, and re-armed by this
#     vector's U-return path;
#   * while running S-mode code, `sscratch` holds 0 (set by `init_traps`
#     at boot and by this vector on every U->S entry).
#
# The entry swap therefore distinguishes the two trap directions: a trap
# from U-mode lands a non-zero anchor address in `sp`, while a nested
# trap from S-mode (a timer/IPI taken while running kernel code) lands 0
# and is recovered onto the interrupted kernel `sp`. The return path
# restores `sscratch` per `sstatus.SPP`: returning to U re-arms it with
# this task's anchor; returning to S leaves it 0.
#
# # Why the anchor carries the kernel `tp`
#
# `tp` (x4) is this port's per-hart anchor — `smp::current_hartid` and the
# Arch-HAL per-CPU slice read the running hart's identity out of it — and it
# is also an ordinary *unprivileged* register U-mode code may write at will
# (it is the RISC-V psABI thread pointer). A vector that left `tp` alone
# would therefore hand the kernel a caller-chosen CPU identity on every
# `ecall`: a task could name another hart and steer the kernel onto that
# core's per-CPU state (its resume handle, dispatch slot, live address
# space), which is kernel memory corruption, not merely a wrong reading.
# So the from-U prologue spills the user's `tp` straight into the frame and
# reloads the kernel's from the anchor *before* any other register is
# touched and long before the Rust handler runs; the U-return path restores
# the user's value, so U-mode keeps its own thread pointer across every
# trap and every context switch (the frame lives on that task's own kernel
# stack). The anchor is written only by code already running on the hart it
# names, so a migrated task always re-enters U-mode under the correct id.
#
# # `sepc`/`sstatus`/`sp` are frame-resident
#
# Saving the three return-state values into the per-trap frame makes each
# exception's resume self-contained across a cooperative context switch:
# a task parked mid-handler resumes at its own `sepc`/`sstatus`/user `sp`,
# not whatever the live CSRs hold after another task ran (the riscv64
# sibling of the aarch64 `ELR_EL1`/`SPSR_EL1`/`SP_EL0` errata fix).
#
# SAFETY-INVARIANTs:
#   1. `.align 2` keeps the vector 4-byte aligned so `stvec` direct mode
#      (mode bits = 0) addresses it correctly.
#   2. The frame is 16-byte aligned (256 bytes) per the riscv64 ABI; the
#      `offset_of!` asserts in `syscall_entry_tests.rs` pin every field
#      offset against the stores/loads below, and the `.equ` values here
#      are parsed and compared against their Rust counterparts by the
#      `trap_layout_tests.rs` pinning tests, so neither side can drift.
#   3. Wherever `sscratch` is armed (non-zero), S-mode interrupts are
#      masked. The entry swap is the only from-U/from-S discriminator, so
#      an S-mode interrupt taken with `sscratch` armed is misread as a
#      U-mode trap: it builds its frame on the armed top, clobbering the
#      interrupted frame, and returns down the S path, which does not
#      re-arm — leaving the task running in U-mode with no kernel stack
#      armed. Both arming sites hold the line: the U-return below writes
#      the frame's saved `sstatus` (hardware cleared `SIE` on entry, so
#      the saved copy always has it clear) before its `csrw sscratch`, and
#      `userentry::enter_user` clears `SIE` before its own arm.
#      A syscall body *does* deliberately enable `SIE` mid-handler so a
#      long `ecall` cannot monopolise the hart; that is safe only because
#      `sscratch` is 0 for the whole handler, which is why the
#      `csrw sscratch, zero` below is not optional.
#   4. No instruction between the entry swap and the from-U reload of `tp`
#      reads `tp`, and nothing after that reload writes it before the
#      return path, so the kernel never observes a U-mode-supplied thread
#      pointer. The anchor is kernel-only memory (the task's own kernel
#      stack), so U-mode can neither read nor forge the kernel `tp` it
#      holds. `trap_layout_tests.rs` pins this ordering against this file.

.equ TRAP_FRAME_SIZE, 256
.equ OFF_SEPC,    224
.equ OFF_SSTATUS, 232
.equ OFF_USP,     240
.equ OFF_UTP,     248

# Byte width of the per-task trap anchor `sscratch` points at while the task
# runs in U-mode, and the offset within it of the hart's kernel `tp`. Sixteen
# bytes (one word plus ABI alignment padding) keeps the frame built below it
# 16-byte aligned.
.equ TRAP_ANCHOR_BYTES, 16
.equ OFF_ANCHOR_KTP, 0

# Offset of the frame's `tp` slot relative to the *pre-adjustment* `sp` (the
# anchor address on the from-U path, the interrupted kernel `sp` on the
# from-S path). Lets each prologue spill the interrupted `tp` into the
# frame-to-be before `sp` is moved, which is what keeps the user's `tp`
# out of the kernel's hands entirely.
.equ OFF_UTP_PRE, OFF_UTP - TRAP_FRAME_SIZE

# Save the caller-saved integer registers into the frame at `sp`.
.macro SAVE_GPRS
    sd      ra, 0(sp)
    sd      t0, 8(sp)
    sd      t1, 16(sp)
    sd      t2, 24(sp)
    sd      t3, 32(sp)
    sd      t4, 40(sp)
    sd      t5, 48(sp)
    sd      t6, 56(sp)
    sd      a0, 64(sp)
    sd      a1, 72(sp)
    sd      a2, 80(sp)
    sd      a3, 88(sp)
    sd      a4, 96(sp)
    sd      a5, 104(sp)
    sd      a6, 112(sp)
    sd      a7, 120(sp)
    # Callee-saved set (s0=fp .. s11): saved so the user-fault crash
    # backtrace can follow the frame-pointer chain from s0. The Rust
    # handler preserves these per the C ABI, so restoring the saved copies
    # is a correct no-op; saving them makes the faulting frame complete.
    sd      s0, 128(sp)
    sd      s1, 136(sp)
    sd      s2, 144(sp)
    sd      s3, 152(sp)
    sd      s4, 160(sp)
    sd      s5, 168(sp)
    sd      s6, 176(sp)
    sd      s7, 184(sp)
    sd      s8, 192(sp)
    sd      s9, 200(sp)
    sd      s10, 208(sp)
    sd      s11, 216(sp)
.endm

# Restore the caller-saved integer registers from the frame at `sp`.
.macro RESTORE_GPRS
    ld      ra, 0(sp)
    ld      t0, 8(sp)
    ld      t1, 16(sp)
    ld      t2, 24(sp)
    ld      t3, 32(sp)
    ld      t4, 40(sp)
    ld      t5, 48(sp)
    ld      t6, 56(sp)
    ld      a0, 64(sp)
    ld      a1, 72(sp)
    ld      a2, 80(sp)
    ld      a3, 88(sp)
    ld      a4, 96(sp)
    ld      a5, 104(sp)
    ld      a6, 112(sp)
    ld      a7, 120(sp)
    ld      s0, 128(sp)
    ld      s1, 136(sp)
    ld      s2, 144(sp)
    ld      s3, 152(sp)
    ld      s4, 160(sp)
    ld      s5, 168(sp)
    ld      s6, 176(sp)
    ld      s7, 184(sp)
    ld      s8, 192(sp)
    ld      s9, 200(sp)
    ld      s10, 208(sp)
    ld      s11, 216(sp)
    # The interrupted thread pointer: the user's own `tp` on a U-mode
    # return, or the (unchanged) kernel `tp` on a nested S-mode return.
    # The U-return path publishes the kernel `tp` into the anchor *before*
    # this runs, so the value it needs is still live when it does.
    ld      tp, OFF_UTP(sp)
.endm

.section .text
.align 2
.global tairix_riscv64_trap_vector
tairix_riscv64_trap_vector:
    # Swap `sp` with `sscratch`. From U-mode `sp` now holds this task's
    # trap-anchor address (non-zero) and `sscratch` holds the user `sp`;
    # from S-mode `sp` holds 0 (the S-mode invariant) and `sscratch` holds
    # the interrupted kernel `sp`.
    csrrw   sp, sscratch, sp
    bnez    sp, 1f
    # Nested S-mode trap: recover the interrupted kernel `sp` from
    # `sscratch`. `sscratch` is left holding that kernel `sp` for now and
    # forced back to the S-mode invariant (0) once the frame is built.
    # `tp` is already this hart's kernel value, so spilling it into the
    # frame-to-be simply makes the frame complete.
    csrr    sp, sscratch
    sd      tp, OFF_UTP_PRE(sp)
    j       2f
1:
    # From U-mode, with `sp` holding the anchor address. `tp` still holds
    # the *user's* thread pointer, which U-mode code may have set to
    # anything: spill it into the frame-to-be and reload this hart's kernel
    # `tp` from the anchor, before any other register is touched. Every
    # later read of `tp` — the Rust handler's per-CPU resolution included —
    # therefore sees the kernel's own hart identity, never a caller-chosen
    # one.
    sd      tp, OFF_UTP_PRE(sp)
    ld      tp, OFF_ANCHOR_KTP(sp)
2:
    # Build the per-trap frame on the kernel stack.
    addi    sp, sp, -TRAP_FRAME_SIZE
    SAVE_GPRS

    # Save the return-state CSRs. `t0` is already spilled, so it is free.
    csrr    t0, sepc
    sd      t0, OFF_SEPC(sp)
    csrr    t0, sstatus
    sd      t0, OFF_SSTATUS(sp)
    # The interrupted `sp` is whatever `sscratch` now holds (the user
    # `sp` for a U-mode trap, or the kernel `sp` for a nested S-mode
    # trap — unused on the S-return path).
    csrr    t0, sscratch
    sd      t0, OFF_USP(sp)
    # Re-establish the S-mode `sscratch == 0` invariant for the duration
    # of the handler (so any nested trap is recognised as from-S).
    csrw    sscratch, zero

    # Pass the saved-frame pointer (sp) to the Rust handler as its first
    # argument so it can read the user's `ecall` registers, write the
    # syscall return value back into the saved a0 slot, and advance the
    # saved `sepc` past the `ecall`.
    mv      a0, sp
    call    tairix_riscv64_trap_handler

    # Restore the return-state CSRs the handler may have updated.
    ld      t0, OFF_SSTATUS(sp)
    csrw    sstatus, t0
    ld      t1, OFF_SEPC(sp)
    csrw    sepc, t1

    # Decide the return target from `sstatus.SPP` (bit 8): set means the
    # trap came from S-mode, clear means from U-mode.
    andi    t1, t0, (1 << 8)
    bnez    t1, 3f

    # Returning to U-mode: publish *this* hart's kernel `tp` into the
    # task's trap anchor (the frame base plus the frame size) and re-arm
    # `sscratch` with the anchor for the next U->S trap, then restore the
    # integer registers — including the user's own `tp` — and load the
    # user `sp` last. `tp` is still the kernel's here, which is why the
    # publish precedes `RESTORE_GPRS`; a task resumed on a different hart
    # therefore re-enters U-mode under that hart's true identity.
    addi    t1, sp, TRAP_FRAME_SIZE
    sd      tp, OFF_ANCHOR_KTP(t1)
    csrw    sscratch, t1
    RESTORE_GPRS
    ld      sp, OFF_USP(sp)
    sret

3:
    # Returning to S-mode (nested trap): `sscratch` stays 0, the kernel
    # `sp` is simply the frame base plus the frame size.
    RESTORE_GPRS
    addi    sp, sp, TRAP_FRAME_SIZE
    sret
