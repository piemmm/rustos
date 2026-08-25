# TAIRiX riscv64 secondary-hart entry trampoline for the QEMU `virt`
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
# a private slice of a secondary-stack pool, indexed by hartid, before
# calling into Rust. The pool is **not** a fixed `.bss` reserve (which
# would cap the machine at a compile-time hart count).
# Instead the boot hart publishes the pool it sized for the machine's
# discovered hart count through `smp::SecondaryStackPool::register`, which
# writes the pool base and the per-hart slice's log2 size into the
# `SECONDARY_STACK_BASE` / `SECONDARY_STACK_SHIFT_BITS` globals below
# before it issues any `hart_start`. This stub reads those globals to
# locate its slice; the `register` call's `fence` (and the SBI
# `hart_start` firmware barrier) order the publish ahead of this hart's
# first read.
#
# SAFETY-INVARIANTs:
#   1. Entered in S-mode with paging off, exactly once per secondary
#      hart, only after the boot hart issued `hart_start` for it.
#   2. `a0` carries this hart's id; the launcher guarantees
#      `a0 < SECONDARY_STACK_COUNT` (the registered pool's hart count), so
#      the per-hart stack slice this stub selects
#      (`base + (hartid + 1) << shift`) lies inside the registered pool.
#   3. `SECONDARY_STACK_BASE`/`_SHIFT_BITS` were published (base
#      non-zero) by the boot hart's `register` before any `hart_start`;
#      a `hart_start` is refused unless a pool is registered, so this
#      stub never reads a null base.
#   4. `tp` is set to the hartid so `smp::current_hartid` reads it back.
#   5. `tairix_arch_riscv64_secondary_main` is `-> !` and never returns;
#      the trailing `wfi` park is defensive.

.section .text, "ax"
.global _start_secondary
_start_secondary:
    # tp = hartid so `current_hartid` can recover this hart's identity
    # from a per-CPU register without re-reading the SBI hand-off.
    mv      tp, a0

    # sp = base + (hartid + 1) << shift, where `base` and `shift` are the
    # runtime-published pool start and per-hart slice's log2 byte size.
    # Each hart owns a `(1 << shift)`-byte slice; the stack grows down
    # from the top of its slice, so hart h uses the top of slot index h.
    # The slice size is a power of two, so the multiply is a left shift
    # (avoiding the `M` multiply extension in this freestanding stub).
    la      t0, SECONDARY_STACK_BASE
    ld      t1, 0(t0)                       # pool base
    la      t0, SECONDARY_STACK_SHIFT_BITS
    ld      t2, 0(t0)                       # per-hart slice log2 size
    addi    t3, a0, 1                       # slot index + 1 (top of slice)
    sll     t3, t3, t2                      # (hartid + 1) << shift
    add     sp, t1, t3

    # Hand this hart's id to the Rust secondary entry. It does not
    # return.
    call    tairix_arch_riscv64_secondary_main

    # Defensive park (unreachable: the Rust entry never returns).
1:
    wfi
    j       1b
