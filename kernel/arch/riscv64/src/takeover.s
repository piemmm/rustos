# TAIRiX riscv64 machine-takeover stack-switch trampoline.
#
# `plans/NEW-SUPERVISOR.md` §9 Stage B (riscv64). This is the one small,
# register-only assembly fragment the machine-takeover needs that cannot be
# expressed in Rust: the switch onto the reserved stack before the
# architecture-neutral whole-RAM sweep runs.
#
# The sweep must not run on a stack the sweep itself destroys (the caller's
# stack is a kernel-service kthread stack allocated from *usable* RAM, which
# the sweep overwrites). This trampoline installs the reserved (`.bss`,
# never-swept) stack the takeover reserves and tail-calls the Rust
# continuation on it, so the sweep's own frames live in reserved memory. It
# never returns (the continuation is `-> !`: it runs the sweep, which loops
# until the operator resets the board, and otherwise parks the hart).
#
# Entry contract (System V riscv64 integer registers):
#   a0 = thin pointer to the caller's `&mut dyn FnMut()` sweep handle
#        (passed straight through to the continuation)
#   a1 = top of the reserved stack (16-byte aligned, grows down)
.section .text, "ax"
.balign 4
.global _takeover_switch_stack
_takeover_switch_stack:
    mv      sp, a1
    tail    tairix_arch_riscv64_takeover_continue
