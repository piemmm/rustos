// TAIRiX x86_64 machine-takeover stack-switch trampoline.
//
// `plans/NEW-SUPERVISOR.md` §9 Stage B (x86_64). This is the one small,
// register-only assembly fragment the machine-takeover needs that cannot be
// expressed in Rust: the switch onto the reserved stack before the
// architecture-neutral whole-RAM sweep runs.
//
// AT&T syntax to match the rest of the x86_64 port (`boot.s`, `context.s`).
//
// The sweep must not run on the caller's stack (a kernel-service kthread
// stack allocated from *usable* RAM, which the sweep destroys). This
// trampoline installs the reserved (`.bss`, never-swept) takeover stack and
// tail-calls the Rust continuation on it, so the sweep's own frames live in
// reserved memory. It never returns (the continuation is `-> !`: it installs
// the boot page tables, runs the sweep — which loops until the operator
// resets the machine — and otherwise parks the CPU).
//
// Entry contract (System V AMD64):
//   %rdi = thin pointer to the caller's `&mut dyn FnMut()` sweep handle
//          (passed straight through to the continuation as its 1st argument)
//   %rsi = top of the reserved stack (16-byte aligned, grows down)
.section .text, "ax"
.code64
.balign 16
.global _takeover_switch_stack
.type _takeover_switch_stack, @function
_takeover_switch_stack:
    movq    %rsi, %rsp                              // install the reserved stack
    // %rdi already holds the thin sweep-handle pointer (1st SysV argument).
    // `call` (not `jmp`) so the continuation sees the ABI-required 16-byte
    // stack alignment at entry; it is `-> !`, so the return is never taken.
    call    tairix_arch_x86_64_takeover_continue
    // Defensive: the continuation never returns.
1:
    hlt
    jmp     1b
.size _takeover_switch_stack, . - _takeover_switch_stack
