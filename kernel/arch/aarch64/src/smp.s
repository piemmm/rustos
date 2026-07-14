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
//      only after the boot core issued `CPU_ON` for it. The known MMU-off
//      `SCTLR_EL1` (`paging::SCTLR_MMU_OFF`) is written before the first
//      data access, so the architecturally UNKNOWN PSCI-entry reset state
//      (EE, WXN, A, SA, …) never governs an access.
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

    // SCTLR_EL1 is architecturally UNKNOWN at PSCI `CPU_ON` entry on
    // real silicon (QEMU resets it benignly): establish the known
    // MMU-off value — 0x30D0_0800, the ARMv8.0 RES1 bits only, i.e.
    // `rustos_arch_aarch64::paging::SCTLR_MMU_OFF` (unit-test-pinned) —
    // before the pool loads below, exactly as `boot.s` `.Lin_el1` does.
    mov     x0, #0x0800
    movk    x0, #0x30D0, lsl #16
    msr     sctlr_el1, x0
    isb

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

// Spin-table release entry (`smp::start_secondary_spintable`).
//
// A core released through a Devicetree spin-table — the Pi 4's stock
// firmware, whose parked cores poll a release word and `br` to the
// address written there — arrives here instead of the PSCI trampoline
// above. The release carries **no** context register (unlike PSCI's
// x0 = context_id) and, on the Pi firmware hand-off, may arrive at EL2,
// so this stub:
//
//   1. drops to EL1 through the shared `_el2_establish_and_drop`
//      routine in `boot.s` (writing every UNKNOWN EL2 control register
//      whole, exactly as the boot core's own drop does) when entered at
//      EL2, and proceeds directly when already at EL1;
//   2. establishes the known MMU-off `SCTLR_EL1` before the first EL1
//      data access;
//   3. recovers its dense CpuId by matching its own `MPIDR_EL1`
//      affinity (`Aff0`–`Aff2`, `smp::MPIDR_AFFINITY_MASK`) against the
//      table `smp::register_secondary_affinities` published in
//      `SECONDARY_AFFINITY_BASE`/`SECONDARY_AFFINITY_COUNT`;
//   4. joins `_start_secondary_aarch64` above with the dense id in x0,
//      sharing the one stack-selection + Rust hand-off path.
//
// SAFETY-INVARIANTs (audited per AGENTS.md §1):
//   1. Entered with the MMU off, at EL1 or EL2, only after
//      `start_secondary_spintable` validated that the secondary entry,
//      the stack pool, and the affinity table were all published (and
//      swept to the point of coherency) before the release word was
//      written — so every symbol read below observes published values.
//   2. A core whose affinity is not in the published table parks in a
//      `wfe` loop (fail closed): an undescribed core must never select
//      a stack slice or enter Rust.
//   3. Bring-up is serialised to one core per release by the
//      target-affinity gate below (`.Lspintable_gate`): a core proceeds
//      only when `SECONDARY_KERNEL_RELEASE_TARGET` names its own
//      affinity, otherwise it re-parks. Both release channels converge
//      on this trampoline — a core the firmware released from its own
//      spin table, and a core the `boot.s` park loop branched in — and a
//      firmware spin-table release must NOT be trusted to wake only one
//      core (a shared/aliased `cpu-release-addr`, or one `sev` waking
//      every parked core, can deliver several here at once). The gate is
//      what actually enforces "one core in bring-up at a time"; relying
//      on the firmware release channel being per-core is a firmware
//      assumption the kernel does not get to make. The affinity-table
//      scan then recovers the released core's dense id (the release
//      carries no context register); an affinity absent from the table
//      parks (fail closed), and the dense map is duplicate-free by
//      construction, so no two cores ever resolve the same stack slice.
.global _start_secondary_spintable_aarch64
_start_secondary_spintable_aarch64:
    // Mask all interrupts until the common path installs vectors.
    msr     DAIFSet, #0xf

    // Drop to EL1 first if the firmware handed this core over at EL2
    // (bits [3:2] of CurrentEL); the boot core's own EL2 path is shared.
    mrs     x1, CurrentEL
    lsr     x1, x1, #2
    cmp     x1, #2
    b.ne    .Lspintable_el1
    adr     x20, .Lspintable_el1
    b       _el2_establish_and_drop

.Lspintable_el1:
    // Known MMU-off SCTLR_EL1 (`paging::SCTLR_MMU_OFF`, unit-test-pinned)
    // before the first EL1 data access, exactly as `boot.s` `.Lin_el1`
    // and the PSCI trampoline above do.
    mov     x0, #0x0800
    movk    x0, #0x30D0, lsl #16
    msr     sctlr_el1, x0
    isb

    // This core's affinity (Aff0–Aff2, `smp::MPIDR_AFFINITY_MASK`).
    mrs     x2, mpidr_el1
    mov     x3, #0xffff
    movk    x3, #0xff, lsl #16
    and     x2, x2, x3

    // Per-core release gate: proceed only when this core is the single
    // secondary the boot CPU is releasing right now
    // (`SECONDARY_KERNEL_RELEASE_TARGET` == this core's affinity). A
    // firmware spin-table release can deliver more than one parked core
    // into this trampoline at once (a shared/aliased release word, or one
    // `sev` waking every parked core); without this gate they would race
    // through the concurrent MMU-adopt / GIC-init path together and the
    // highest dense id intermittently faults mid-bring-up and never
    // checks in (the real-metal Raspberry Pi 4 symptom). Re-park in a
    // low-power `wfe` loop and re-read the target on every event until it
    // names this core; the boot CPU stores the target (swept to the point
    // of coherency) immediately before the `sev` that releases each core,
    // so exactly one core is ever past this gate at a time. The all-ones
    // initial target matches no real affinity, so the gate stays closed
    // until a deliberate release opens it (fail closed). Read with the MMU
    // (and cache) off, straight from RAM, exactly as the `boot.s` park
    // loop reads it; x7 is scratch, x2 (this core's affinity) is kept.
    // The predicate is `smp::release_gate_open` (target == own affinity),
    // the one definition this and the boot.s park loop both compile to.
.Lspintable_gate:
    adrp    x7, SECONDARY_KERNEL_RELEASE_TARGET
    add     x7, x7, :lo12:SECONDARY_KERNEL_RELEASE_TARGET
    ldr     x7, [x7]
    cmp     x7, x2
    b.eq    .Lspintable_scan_start          // this core's turn: bring up
    wfe
    b       .Lspintable_gate                // not yet: re-park, re-check

.Lspintable_scan_start:
    // Published dense-id → affinity table (base 0 = unpublished: park).
    adrp    x3, SECONDARY_AFFINITY_BASE
    add     x3, x3, :lo12:SECONDARY_AFFINITY_BASE
    ldr     x3, [x3]
    cbz     x3, .Lspintable_park
    adrp    x4, SECONDARY_AFFINITY_COUNT
    add     x4, x4, :lo12:SECONDARY_AFFINITY_COUNT
    ldr     x4, [x4]

    // Linear match: dense id x5 in 0..count with table[x5] == affinity.
    mov     x5, #0
.Lspintable_scan:
    cmp     x5, x4
    b.hs    .Lspintable_park            // not described: park, fail closed
    ldr     x6, [x3, x5, lsl #3]
    cmp     x6, x2
    b.eq    .Lspintable_found
    add     x5, x5, #1
    b       .Lspintable_scan

.Lspintable_found:
    // Join the common trampoline with the dense id in x0 (it re-masks
    // DAIF and re-writes SCTLR_EL1 — both idempotent).
    mov     x0, x5
    b       _start_secondary_aarch64

.Lspintable_park:
    wfe
    b       .Lspintable_park
