# RustOS riscv64 secondary-hart entry trampoline for the QEMU `virt`
# board.
#
# A secondary hart is parked in OpenSBI (M-mode) until the boot hart
# asks for it through the SBI HSM `hart_start` call (see `sbi::hart_start`
# and `smp::start_secondary`). OpenSBI then enters this S-mode label with
# the HSM hand-off registers set:
#
#   a0 = hartid of this (just-started) hart
#   a1 = opaque value the `hart_start` caller passed (unused here)
#
# Unlike the boot hart, a secondary hart has no stack: this stub gives it
# a private slice of the `.bss` stack pool, indexed by hartid, before
# calling into Rust. The `.bss` is already zeroed by the boot hart's
# `boot.s` memset, which runs to completion before any `hart_start` is
# issued, so the pool is clear.
#
# SAFETY-INVARIANTs:
#   1. Entered in S-mode with paging off, exactly once per secondary
#      hart, only after the boot hart issued `hart_start` for it.
#   2. `a0` carries this hart's id; the launcher guarantees
#      `a0 < SECONDARY_MAX_HARTS`, so the per-hart stack slice it selects
#      lies inside the reserved pool.
#   3. `tp` is set to the hartid so `smp::current_hartid` reads it back.
#   4. `rustos_arch_riscv64_secondary_main` is `-> !` and never returns;
#      the trailing `wfi` park is defensive.

.equ SECONDARY_MAX_HARTS, 8
.equ SECONDARY_STACK_SHIFT, 14
.equ SECONDARY_STACK_SIZE, (1 << SECONDARY_STACK_SHIFT)

.section .text, "ax"
.global _start_secondary
_start_secondary:
    # tp = hartid so `current_hartid` can recover this hart's identity
    # from a per-CPU register without re-reading the SBI hand-off.
    mv      tp, a0

    # sp = __secondary_stacks + (hartid + 1) * SECONDARY_STACK_SIZE.
    # Each hart owns a SECONDARY_STACK_SIZE slice; the stack grows down
    # from the top of its slice, so hart h uses slot index h. The slice
    # size is a power of two, so the multiply is a left shift (avoiding
    # the `M` multiply extension in this freestanding stub).
    la      t0, __secondary_stacks
    slli    t2, a0, SECONDARY_STACK_SHIFT
    li      t1, SECONDARY_STACK_SIZE
    add     t2, t2, t1
    add     sp, t0, t2

    # Hand this hart's id to the Rust secondary entry. It does not
    # return.
    call    rustos_arch_riscv64_secondary_main

    # Defensive park (unreachable: the Rust entry never returns).
1:
    wfi
    j       1b

# Per-hart secondary stack pool. Lives in `.bss` so the boot hart's
# memset zeroes it; SECONDARY_MAX_HARTS * SECONDARY_STACK_SIZE bytes.
.section .bss.secondary_stacks, "aw", @nobits
.balign 16
__secondary_stacks:
    .skip SECONDARY_MAX_HARTS * SECONDARY_STACK_SIZE
__secondary_stacks_top:
