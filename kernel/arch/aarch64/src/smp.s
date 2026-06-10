// RustOS aarch64 secondary-core entry trampoline.
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
// a private slice of a secondary-stack pool, indexed by the context id,
// before calling into Rust. The pool is **not** a fixed `.bss` reserve
// (which would cap the machine at a compile-time core count, `AGENTS.md`
// §24.1). Instead the boot core publishes the pool it sized for the
// machine's discovered core count through `smp::SecondaryStackPool::
// register`, which writes the pool base and per-core stride into the
// `SECONDARY_STACK_BASE` / `SECONDARY_STACK_STRIDE` globals below before
// it issues any `CPU_ON`. This stub reads those globals to locate its
// slice; the `register` call's `dsb sy` (and the PSCI `CPU_ON` firmware
// barrier) order the publish ahead of this core's first read.
//
// SAFETY-INVARIANTs (audited per AGENTS.md §1):
//   1. Entered at EL1 with the MMU off, exactly once per secondary core,
//      only after the boot core issued `CPU_ON` for it.
//   2. x0 carries this core's dense CpuId; `smp::start_secondary`
//      guarantees `x0 < SECONDARY_STACK_COUNT` (the registered pool's
//      core count), so the per-core stack slice this stub selects
//      (`base + (cpuid + 1) * stride`) lies inside the registered pool.
//   3. `SECONDARY_STACK_BASE`/`_STRIDE` were published (non-zero) by the
//      boot core's `register` before any `CPU_ON`; a `CPU_ON` is refused
//      unless a pool is registered, so this stub never reads a null base.
//   4. `rustos_arch_aarch64_secondary_main` is `-> !` and never returns;
//      the trailing `wfi` park is defensive.

.section .text, "ax"
.global _start_secondary_aarch64
_start_secondary_aarch64:
    // Mask all interrupts (D, A, I, F) until this core installs its
    // vector table and is ready to take them.
    msr     DAIFSet, #0xf

    // Preserve the dense CpuId (context id) across the stack setup.
    mov     x19, x0

    // sp = base + (cpuid + 1) * stride, where `base` and `stride` are the
    // runtime-published pool start and per-core slice size. Each core
    // owns a `stride`-byte slice; the stack grows down from the top of
    // its slice, so core `c` uses the top of slot index `c`.
    adrp    x0, SECONDARY_STACK_BASE
    add     x0, x0, :lo12:SECONDARY_STACK_BASE
    ldr     x0, [x0]                        // pool base
    adrp    x1, SECONDARY_STACK_STRIDE
    add     x1, x1, :lo12:SECONDARY_STACK_STRIDE
    ldr     x1, [x1]                        // per-core stride (bytes)
    add     x2, x19, #1                     // slot index + 1 (top of slice)
    madd    x0, x2, x1, x0                  // x0 = base + (cpuid + 1) * stride
    mov     sp, x0

    // Hand this core's dense id to the Rust secondary entry. It does not
    // return.
    mov     x0, x19
    bl      rustos_arch_aarch64_secondary_main

    // Defensive park (unreachable: the Rust entry never returns).
1:
    wfi
    b       1b
