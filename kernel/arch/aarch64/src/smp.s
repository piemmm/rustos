// RustOS aarch64 secondary-core entry trampoline for the QEMU `virt`
// board.
//
// A secondary core is parked in firmware (powered off) until the boot
// core asks for it through PSCI `CPU_ON` (see `psci::cpu_on` and
// `smp::start_secondary`). The firmware then enters this label, in the
// same exception level as the caller (EL1 on the `virt` board), with the
// MMU off and the PSCI hand-off register set:
//
//   x0 = context_id the `CPU_ON` caller passed (the dense CpuId here)
//
// Unlike the boot core, a secondary core has no stack: this stub gives it
// a private slice of the `.bss` stack pool, indexed by the context id,
// before calling into Rust. The pool lives in `.bss`, already zeroed by
// the boot core's `boot.s` clear loop, which runs to completion before
// any `CPU_ON` is issued.
//
// SAFETY-INVARIANTs (audited per AGENTS.md §1):
//   1. Entered at EL1 with the MMU off, exactly once per secondary core,
//      only after the boot core issued `CPU_ON` for it.
//   2. x0 carries this core's dense CpuId; the launcher guarantees
//      `x0 < SECONDARY_MAX_CPUS`, so the per-core stack slice it selects
//      lies inside the reserved pool.
//   3. `rustos_arch_aarch64_secondary_main` is `-> !` and never returns;
//      the trailing `wfi` park is defensive.

.equ SECONDARY_MAX_CPUS, 8
.equ SECONDARY_STACK_SHIFT, 16          // 64 KiB per core (1 << 16).
.equ SECONDARY_STACK_SIZE, (1 << SECONDARY_STACK_SHIFT)

.section .text, "ax"
.global _start_secondary_aarch64
_start_secondary_aarch64:
    // Mask all interrupts (D, A, I, F) until this core installs its
    // vector table and is ready to take them.
    msr     DAIFSet, #0xf

    // Preserve the dense CpuId (context id) across the stack setup.
    mov     x19, x0

    // sp = __secondary_stacks_aarch64 + (cpuid + 1) * SECONDARY_STACK_SIZE.
    // Each core owns a SECONDARY_STACK_SIZE slice; the stack grows down
    // from the top of its slice, so core `c` uses slot index `c`. The
    // slice size is a power of two, so the multiply is a left shift.
    adrp    x0, __secondary_stacks_aarch64
    add     x0, x0, :lo12:__secondary_stacks_aarch64
    lsl     x1, x19, #SECONDARY_STACK_SHIFT
    mov     x2, #SECONDARY_STACK_SIZE
    add     x1, x1, x2
    add     x0, x0, x1
    mov     sp, x0

    // Hand this core's dense id to the Rust secondary entry. It does not
    // return.
    mov     x0, x19
    bl      rustos_arch_aarch64_secondary_main

    // Defensive park (unreachable: the Rust entry never returns).
1:
    wfi
    b       1b

// Per-core secondary stack pool. Lives in `.bss` so the boot core's
// clear loop zeroes it; SECONDARY_MAX_CPUS * SECONDARY_STACK_SIZE bytes.
.section .bss.secondary_stacks_aarch64, "aw", %nobits
.balign 16
__secondary_stacks_aarch64:
    .skip SECONDARY_MAX_CPUS * SECONDARY_STACK_SIZE
__secondary_stacks_aarch64_top:
