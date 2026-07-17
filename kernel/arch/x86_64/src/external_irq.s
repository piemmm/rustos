// x86_64 external-IRQ ISR thunks (Stage 4.D Item 2-tail.2).
//
// Reserves architectural vectors 0x30..=0xFE for external IRQs (the
// usable range above the reserved exception/IPI vectors). Each
// vector points to its own tiny stub that pushes the vector number
// as an immediate, then jumps into a shared trampoline that:
//
//   1. Pushes the 15 architectural GPRs in the order pinned by
//      `interrupts.rs::SavedRegs` (load-bearing — the host-side
//      `saved_regs_layout_is_pinned` test in `interrupts.rs` is the
//      cross-check).
//   2. Loads %rdi with a pointer to the SavedRegs block.
//   3. Loads %rsi with the synthetic "vector" qword the per-vector
//      stub pushed before the GPR saves (it sits at `[rsp + 15*8]`).
//   4. Aligns the stack to %rsp ≡ 8 (mod 16) per SysV AMD64 §3.2.2
//      and calls the Rust dispatcher
//      `tairix_arch_x86_64_external_irq_dispatch(SavedRegs*, u64)`.
//   5. Pops the GPRs in reverse order, drops the vector qword, and
//      `iretq`s.
//
// SAFETY-INVARIANTs (audited per AGENTS.md §10):
//
//   1. None of the targeted vectors (0x30..=0xFE) push a hardware
//      error code, so the synthetic-vector qword sits at a known
//      offset and the per-vector immediate fits in a sign-extended
//      i32 (every value is ≤ 0xFE).
//   2. The stub addresses are published through the
//      `tairix_arch_x86_64_external_irq_table` data array (one
//      `.quad` per vector, indexed by `vector - EXTERNAL_VECTOR_FIRST`).
//      Rust references it via `extern "C"` so the IDT installer in
//      `kernel/tairix-kernel::boot` can take the address of each stub
//      without having to know its name.
//   3. The shared trampoline is `tairix_arch_x86_64_external_irq_common`
//      and is the single chokepoint AGENTS.md §2.2 (no duplication)
//      requires for code that would otherwise be duplicated across
//      207 ISRs.
//   4. The Rust dispatcher must NOT return (or, if it does, the
//      trampoline pops GPRs, drops the vector qword, and `iretq`s back
//      to user space; the dispatcher is documented as performing the
//      LAPIC EOI write before returning).
//   5. The trampoline runs with interrupts already disabled (the IDT
//      entries are interrupt gates at DPL 0), so it does not save
//      RFLAGS itself — the architectural `iretq` restores the
//      pre-interrupt state.

.section .text

// --- Shared trampoline --------------------------------------------
//
// On entry the stack layout is:
//
//   [rsp+0x00]                vector qword (pushed by the per-vector
//                             stub immediately before jumping here)
//   [rsp+0x08 .. rsp+0x28]    CPU-pushed InterruptStackFrame
//                             (rip, cs, rflags, rsp, ss)
//
// The trampoline pushes the 15 GPRs (120 bytes), so on entry to the
// dispatcher %rsi receives the vector and %rdi receives a pointer
// at the saved-regs block. After the call, the trampoline pops the
// 15 GPRs and drops the vector qword (`add rsp, 8`) before `iretq`.

.global tairix_arch_x86_64_external_irq_common
.type   tairix_arch_x86_64_external_irq_common, @function

tairix_arch_x86_64_external_irq_common:
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

    // %rdi <- pointer to SavedRegs block (== %rsp).
    movq    %rsp, %rdi
    // %rsi <- the per-vector immediate the stub pushed before the
    // GPR saves; it sits 15*8 = 120 bytes above SavedRegs.
    movq    120(%rsp), %rsi

    // Alignment: 5 hardware-pushed qwords (40) + 1 vector qword (8) +
    // 15 GPR pushes (120) = 168 bytes, so %rsp ≡ 0 (mod 16) here.
    // SysV AMD64 wants %rsp ≡ 8 (mod 16) at the `call` instruction,
    // so subtract 8.
    subq    $8, %rsp
    call    tairix_arch_x86_64_external_irq_dispatch
    addq    $8, %rsp

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

    // Drop the per-vector immediate that the stub pushed.
    addq    $8, %rsp
    iretq

.size tairix_arch_x86_64_external_irq_common, . - tairix_arch_x86_64_external_irq_common

// --- Per-vector stubs ---------------------------------------------
//
// Generated through `.altmacro` + `.rept` so AGENTS.md §2.2 (no
// duplication) is satisfied. The macro produces one labelled stub
// per vector in [EXTERNAL_VECTOR_FIRST, EXTERNAL_VECTOR_LAST]
// (0x30..=0xFE inclusive — 207 vectors).
//
// Each stub is exactly two instructions: push the vector as an
// immediate, then jmp to the shared trampoline. The `push imm8`
// encoding the GNU assembler emits for values in 0..=0x7F is two
// bytes; values 0x80..=0xFE use `push imm32` and are five bytes.
// The size difference is irrelevant because Rust never assumes a
// fixed stride — the per-vector addresses are published through the
// `tairix_arch_x86_64_external_irq_table` data array below.

.altmacro

.macro external_irq_stub vec
    .global tairix_arch_x86_64_external_irq_\vec
    .type   tairix_arch_x86_64_external_irq_\vec, @function
tairix_arch_x86_64_external_irq_\vec:
    pushq   $\vec
    jmp     tairix_arch_x86_64_external_irq_common
    .size tairix_arch_x86_64_external_irq_\vec, . - tairix_arch_x86_64_external_irq_\vec
.endm

.macro external_irq_table_entry vec
    .quad tairix_arch_x86_64_external_irq_\vec
.endm

.set    vec_no, 0x30
.rept   (0xFF - 0x30)
    external_irq_stub %vec_no
    .set vec_no, vec_no + 1
.endr

// --- Vector table -------------------------------------------------
//
// One `.quad` per vector, in ascending vector order, indexed by
// `vector - EXTERNAL_VECTOR_FIRST`. Published in `.rodata` (the
// addresses never change after link time) so the Rust side can read
// it as `extern "C" static EXTERNAL_VECTOR_TABLE: [usize; EXTERNAL_VECTOR_COUNT]`.
//
// The label-substitution happens through the same `.altmacro` `%vec_no`
// expansion the per-vector stubs use, but `%var` only expands inside
// macro bodies — emitting `.quad` directly with `%vec_no` would leave
// the literal `%vec_no` text in the assembled stream and fail with
// "expected relocatable expression". Wrapping the `.quad` in a single-
// argument macro (`external_irq_table_entry`) puts the label inside a
// macro body, which is the form `.altmacro` is documented to handle.

.section .rodata
.global tairix_arch_x86_64_external_irq_table
.type   tairix_arch_x86_64_external_irq_table, @object

tairix_arch_x86_64_external_irq_table:
.set    vec_no, 0x30
.rept   (0xFF - 0x30)
    external_irq_table_entry %vec_no
    .set vec_no, vec_no + 1
.endr

.size tairix_arch_x86_64_external_irq_table, . - tairix_arch_x86_64_external_irq_table
