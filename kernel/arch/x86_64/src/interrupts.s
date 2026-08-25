// x86_64 common ISR prologue (Stage 3a (c2)).
//
// tairix_arch_x86_64_isr_default:
//     The single fail-closed thunk every default IDT slot points at.
//     The prologue saves the 15 architectural general-purpose registers
//     in the order pinned by `SavedRegs` (see `interrupts.rs`) and calls
//     into `tairix_arch_x86_64_default_interrupt(&mut SavedRegs)`. That
//     Rust callee is `-> !`; reaching the `iretq` below is a kernel
//     bug. Belt-and-braces
//
// SAFETY-INVARIANTS:
//
//   1. Entry from the IDT delivery: the CPU has already pushed the
//      5-word InterruptStackFrame (RIP, CS, RFLAGS, RSP, SS) on the
//      *destination* stack (per the IST/RSP0 selection embedded in the
//      IDT vector that targeted this thunk). %rsp on entry therefore
//      points at the InterruptStackFrame.
//   2. The CPU did **not** push a hardware error code. Vectors that
//      *do* push one (8, 10–14, 17, 21) need vector-specific stubs;
//      the default thunk is documented in `interrupts.rs` as covering
//      only no-error vectors. The Stage 3a (c5) preemption commit will
//      emit per-vector stubs via a `define_isr!` macro.
//   3. The push order below produces the exact in-memory `SavedRegs`
//      layout the Rust `#[repr(C)] struct SavedRegs` pins. The host
//      unit test `saved_regs_layout_is_pinned` is the cross-check.
//   4. After the call returns (which it does not — see below), the
//      epilogue pops the 15 GPRs in reverse order and `iretq`s. The
//      epilogue is dead code today because `default_interrupt` is
//      `-> !`; it is left in place so the (c5) commit can swap in a
//      handler that *does* return without touching the prologue.

.section .text
.global tairix_arch_x86_64_isr_default
.type   tairix_arch_x86_64_isr_default, @function

tairix_arch_x86_64_isr_default:
    // Prologue: push GPRs in the order pinned by SavedRegs.
    // r15 ends up at the *lowest* address (offset 0 of SavedRegs);
    // rax ends up at the *highest* (offset 14*8) — see SavedRegs
    // field order in interrupts.rs.
    pushq   %rax
    pushq   %rcx
    pushq   %rdx
    pushq   %rbx
    pushq   %rbp
    pushq   %rsi
    pushq   %rdi
    pushq   %r8
    pushq   %r9
    pushq   %r10
    pushq   %r11
    pushq   %r12
    pushq   %r13
    pushq   %r14
    pushq   %r15

    // %rdi <- pointer to SavedRegs (which is %rsp).
    movq    %rsp, %rdi

    // 16-byte stack alignment for the SysV call.
    //
    // Intel SDM Vol 3A §6.14.2 guarantees the CPU aligns %rsp to 16
    // before pushing the InterruptStackFrame, so %rsp at thunk entry
    // is 16-byte aligned. After 15 GPR pushes (= 120 bytes), %rsp ≡ 0
    // (mod 16) again (since 120 + 40 frame bytes = 160 ≡ 0 mod 16).
    // System V AMD64 §3.2.2 wants %rsp ≡ 8 (mod 16) at the `call`
    // instruction so that the implicit return-address push lands the
    // callee with a 16-aligned stack. Subtract 8 to satisfy that.
    subq    $8, %rsp

    call    tairix_arch_x86_64_default_interrupt

    // The callee is `-> !`. Reaching here is a kernel bug;
    // belt-and-braces.
    addq    $8, %rsp

    // Epilogue (dead today; kept for the (c5) commit).
    popq    %r15
    popq    %r14
    popq    %r13
    popq    %r12
    popq    %r11
    popq    %r10
    popq    %r9
    popq    %r8
    popq    %rdi
    popq    %rsi
    popq    %rbp
    popq    %rbx
    popq    %rdx
    popq    %rcx
    popq    %rax
    iretq

.size tairix_arch_x86_64_isr_default, . - tairix_arch_x86_64_isr_default
