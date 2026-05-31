# RustOS riscv64 S-mode trap vector for the QEMU `virt` board.
#
# Installed into `stvec` (direct mode) by `trap::init_traps`. Every
# S-mode trap — synchronous exception or interrupt — enters here with
# `sstatus.SIE` cleared by hardware. The vector saves the interrupted
# context's caller-saved integer registers, calls the Rust handler
# (`rustos_riscv64_trap_handler`, which preserves callee-saved registers
# per the C ABI), restores, and returns with `sret`.
#
# Only caller-saved registers are saved: the Rust handler is an
# `extern "C"` function, so the compiler preserves `s0..s11` for us; the
# interrupted code's `gp`/`tp` are not clobbered by the handler. `sp` is
# restored by the symmetric `addi`.
#
# SAFETY-INVARIANTs:
#   1. `.align 2` keeps the vector 4-byte aligned so `stvec` direct mode
#      (mode bits = 0) addresses it correctly.
#   2. The frame is 16-byte aligned (144 bytes) per the riscv64 ABI.
#   3. Interrupts stay disabled for the whole handler (hardware clears
#      `sstatus.SIE` on trap entry; `sret` restores the pre-trap value
#      from `sstatus.SPIE`).

.section .text
.align 2
.global rustos_riscv64_trap_vector
rustos_riscv64_trap_vector:
    addi    sp, sp, -144
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

    # Pass the saved-frame pointer (sp) to the Rust handler as its first
    # argument so it can read the user's `ecall` registers and write the
    # syscall return value back into the saved a0 slot. a0 was already
    # spilled to 64(sp) above, so clobbering it here is safe.
    mv      a0, sp
    call    rustos_riscv64_trap_handler

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
    addi    sp, sp, 144

    sret
