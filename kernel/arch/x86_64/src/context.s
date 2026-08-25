// x86_64 context-switch primitive (Stage 3a (c1)).
//
// extern "C" fn tairix_arch_x86_64_switch(prev: *mut TaskCtx,
//                                          next: *mut TaskCtx);
//
// Calling convention: System V AMD64. %rdi = prev, %rsi = next.
//
// SAFETY-INVARIANTS:
//
//   1. The function is called with `prev` and `next` non-null. The
//      Rust-side safe wrapper `crate::context::switch` documents this
//      contract; violating it is undefined behaviour by design.
//   2. `TaskCtx` has `repr(C)` layout pinned by a const-assert in
//      `context.rs` to `{{ rsp: u64 }} at offset 0`. The `0x00(%rdi)` /
//      `0x00(%rsi)` operands below address that field.
//   3. The frame this routine produces on suspend / consumes on resume
//      matches `TaskCtx::prepare` byte-for-byte; the host test
//      `prepare_writes_initial_frame` is the canonical cross-check.
//   4. On entry we are in long mode, CPL=0, with the per-CPU GDT
//      installed by `crate::percpu::init` — `switch` does not touch
//      segment registers because no segment swap is required between
//      two ring-0 kernel tasks (ring-0→ring-3 transitions are the
//      syscall/sysret commit's job, not this routine's).
//   5. Interrupts may be enabled. We make no atomic guarantees about
//      *delivery* of an interrupt across the switch: the caller is
//      responsible for masking interrupts (e.g. via CLI / STI around
//      the call) if it needs the switch to be uninterruptible.

.section .text
.global tairix_arch_x86_64_switch
.type   tairix_arch_x86_64_switch, @function

tairix_arch_x86_64_switch:
    // --- Suspend half ---
    // Push callee-saved registers onto the *outgoing* task's stack so a
    // popq sequence later restores them. Stack grows downward and `rdi`
    // is pushed last, so the resulting frame the resume half pops is, in
    // ascending address order (lowest = last pushed = first popped):
    //
    //   [rsp + 0x00]  rdi  (= prev pointer at entry — see below)
    //   [rsp + 0x08]  r15
    //   [rsp + 0x10]  r14
    //   [rsp + 0x18]  r13
    //   [rsp + 0x20]  r12
    //   [rsp + 0x28]  rbx
    //   [rsp + 0x30]  rbp
    //   [rsp + 0x38]  return address pushed by the call site
    //
    // `TaskCtx::prepare` seeds a synthetic copy of exactly this frame
    // (with `rdi` = the task's first-run argument and the return address
    // = its entry point), so its slot order must match this `popq`
    // order, not the textual push order below.
    //
    // We deliberately save `rdi` (the outbound `prev` pointer) too.
    // The reason: when a freshly-prepared task first runs, its
    // synthesised frame has `rdi` set to that task's first-run argument
    // (see `TaskCtx::prepare`). For a previously-running task being
    // resumed, `rdi` is a caller-saved register whose value is
    // irrelevant per the System V ABI. Saving it unconditionally lets
    // the same epilogue handle both cases.
    pushq   %rbp
    pushq   %rbx
    pushq   %r12
    pushq   %r13
    pushq   %r14
    pushq   %r15
    pushq   %rdi

    // Record outgoing %rsp into prev.rsp.
    //
    // Note: %rdi at this point still holds the original `prev` pointer
    // (we pushed a *copy* above; %rdi was not popped). We write through
    // %rdi here, then immediately consume %rsi.
    movq    %rsp, 0(%rdi)

    // --- Resume half ---
    // Load the inbound task's saved stack pointer.
    movq    0(%rsi), %rsp

    // Pop the inbound task's saved state in the reverse of the suspend
    // sequence. The last popq lands the new %rdi (== inbound task's
    // first-run argument or the saved %rdi from a prior suspend, both
    // of which are documented in the suspend half above).
    popq    %rdi
    popq    %r15
    popq    %r14
    popq    %r13
    popq    %r12
    popq    %rbx
    popq    %rbp

    // `ret` pops the return address the *inbound* task left on its
    // stack — either a synthesised `entry` (first run) or the address
    // immediately after the suspend's `call tairix_arch_x86_64_switch`
    // (resumed run).
    ret

.size tairix_arch_x86_64_switch, . - tairix_arch_x86_64_switch
