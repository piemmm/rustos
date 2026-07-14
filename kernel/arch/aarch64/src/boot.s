// RustOS aarch64 boot trampoline. Board-independent: it serves both the
// QEMU `virt` board and the Raspberry Pi 4 (BCM2711). Only the load
// address differs between boards, and that lives in the per-board linker
// script (`aarch64-virt.ld` / `aarch64-rpi4.ld`) — the `AGENTS.md` §1
// "boot stubs" carve-out (`plans/PI.md` §0.2). Every other board
// difference is discovered device-tree data, never a fork of this stub.
//
// The loader (QEMU `-kernel <elf>`, or the Pi firmware loading
// `kernel8.img` at 0x8_0000) enters `_start` with the Linux aarch64
// boot-protocol hand-off:
//
//   x0 = physical address of the flattened device tree (DTB)
//   x1..x3 = 0 (reserved)
//
// Entry exception level varies by board: the default `virt` machine tops
// out at EL1 (EL2 when `virtualization=on`); the Pi firmware enters at
// EL2. This stub handles both: if entered at EL2 it configures EL1 to run
// AArch64, grants EL1/EL0 the physical counter/timer, zeroes the virtual
// counter offset, and `eret`s to EL1; if already at EL1 it proceeds
// directly. It then establishes a stack, zeroes `.bss` so Rust statics
// start cleared, and tail-calls the Rust entry with the DTB pointer
// preserved in x0.
//
// Secondary-CPU parking: QEMU `virt` holds secondaries in firmware until
// a PSCI `CPU_ON`, so only the boot CPU reaches `_start`. The Pi 4
// firmware (with no `armstub` spin-table) instead releases all four
// cores straight to the kernel entry. To behave correctly on both, every
// CPU whose `MPIDR_EL1` affinity is non-zero parks in a low-power `wfe`
// loop here, before touching the single boot stack or `.bss`, polling
// the kernel spin-table release word (`smp::SECONDARY_KERNEL_RELEASE`):
// it stays parked while the word is zero and branches to the address
// written there when the SMP bring-up (`plans/PI.md` P5) releases it
// deliberately (`smp::start_secondary_spintable`). The release word is
// *shared*, so one `sev` wakes every parked core; a second word,
// `SECONDARY_KERNEL_RELEASE_TARGET`, names the single core being brought
// up now, and a woken core proceeds only when that target equals its own
// affinity — the rest re-park. Bring-up is therefore strictly one core
// at a time (never a concurrent MMU-adopt / GIC-init race). This fails
// closed (`AGENTS.md` §2.1 — never race the secondaries onto a shared
// stack): nothing is released until the boot CPU has published the
// secondary stacks, entry, and affinity table.
//
// SAFETY-INVARIANTs (audited per AGENTS.md §10):
//   1. The body below `_start` runs exactly once, on the boot CPU
//      (affinity 0); every other CPU is trapped in the `wfe` park above
//      it. Interrupts are masked before any handler exists.
//   2. x0 carries the DTB pointer described above; it is preserved across
//      the EL2->EL1 drop in callee-saved x19 and restored before the call.
//   3. `.Lin_el1` writes the known MMU-off `SCTLR_EL1`
//      (`paging::SCTLR_MMU_OFF`) before the first EL1 data access, so the
//      architecturally UNKNOWN EL1 reset state (EE, WXN, A, SA, …) never
//      governs an access on either entry path. Likewise the EL2 path
//      writes every EL2 control register *whole* with its known hand-off
//      value (`el2::HCR_EL2_HANDOFF` and friends, unit-test-pinned) —
//      the Pi firmware stub leaves HCR_EL2/CNTHCTL_EL2/CPTR_EL2/MDCR_EL2
//      at their UNKNOWN reset values, and an UNKNOWN HCR_EL2.TVM traps
//      EL1's first MAIR/TCR/TTBR/SCTLR write into vector-less EL2 — a
//      silent hang at the MMU switch on real silicon (QEMU resets these
//      registers benignly, masking the residue).
//   4. The stack top and `.bss` bounds come from the active linker script
//      (`aarch64-virt.ld` / `aarch64-rpi4.ld`); `__bss_start`/`__bss_end`
//      are 16-byte aligned so the `stp` clear loop is exact.
//   5. `rustos_arch_aarch64_main` is `-> !` and never returns; the trailing
//      `wfi` park is unreachable under QEMU but is the correct conservative
//      behaviour on bare metal.

.section .text.boot, "ax"
.global _start
_start:
    // Mask all interrupts (D, A, I, F) during bring-up.
    msr     DAIFSet, #0xf

    // Preserve the DTB pointer (x0) in callee-saved x19 before any
    // scratch register is touched; it is restored to x0 just before the
    // tail-call into Rust.
    mov     x19, x0

    // Park every non-boot CPU. The boot CPU is the one whose MPIDR_EL1
    // affinity (Aff0..Aff3) is zero; any other core waits in a `wfe`
    // loop until the SMP bring-up releases it explicitly (plans/PI.md
    // P5) by writing the spin-table trampoline's address into
    // `SECONDARY_KERNEL_RELEASE` (`smp::start_secondary_spintable`) —
    // the kernel's own release word, polled exactly like a firmware
    // spin-table mailbox — and only when the published release target
    // matches this core's affinity (the one-core-at-a-time gate below).
    // The release word lives in `.bss`, which the boot CPU zeroes below;
    // a parked core reading a mid-clear value sees only zero (keep
    // parking) or, much later, the published entry — never a torn
    // in-between (the store is a single aligned doubleword). This runs
    // before the shared boot stack or `.bss` is touched by *this* core,
    // so a released-at-reset Pi secondary cannot race the boot CPU.
    // Scratch registers x4..x8 are used so x19 (the DTB) is left
    // untouched (x8 holds this core's affinity across the park loop).
    mrs     x4, mpidr_el1
    mov     x5, #0xffff             // Aff0 [7:0] | Aff1 [15:8]
    movk    x5, #0xff, lsl #16      // | Aff2 [23:16]  => x5 = 0x00FF_FFFF
    and     x6, x4, x5
    ubfx    x7, x4, #32, #8         // Aff3 [39:32]
    orr     x6, x6, x7
    cbz     x6, .Lboot_cpu
    // This core's own masked affinity (Aff0-2, `smp::MPIDR_AFFINITY_MASK`
    // = 0x00FF_FFFF) — the value the boot CPU publishes as the release
    // target for exactly one core at a time. Kept in x8 across the park
    // loop (a firmware `sev` cannot clobber a register).
    and     x8, x4, x5
.Lpark_secondary:
    // Wait for an event, then re-check *both* release words each pass.
    // The kernel release word is shared by every parked core, so one
    // `sev` wakes them all; the release-target word names the single
    // core the boot CPU is bringing up now, so only that core branches
    // and the rest re-park. This serialises secondary bring-up: releasing
    // one core never races the others through the concurrent MMU-adopt /
    // GIC-init path (a coherency hazard that intermittently faulted the
    // last-released core on a real Pi 4). The target compare is the shared
    // `smp::release_gate_open` predicate (target == own affinity), which
    // the `smp.s` spin-table trampoline gate implements identically — a
    // core released straight into this loop re-checks the same gate again
    // when it reaches the trampoline. x8 holds this core's affinity;
    // x4/x5 are scratch.
    wfe
    adrp    x4, SECONDARY_KERNEL_RELEASE
    add     x4, x4, :lo12:SECONDARY_KERNEL_RELEASE
    ldr     x4, [x4]
    cbz     x4, .Lpark_secondary            // gate closed: keep parking
    adrp    x5, SECONDARY_KERNEL_RELEASE_TARGET
    add     x5, x5, :lo12:SECONDARY_KERNEL_RELEASE_TARGET
    ldr     x5, [x5]
    cmp     x5, x8
    b.ne    .Lpark_secondary                // not this core's turn: re-park
    br      x4

.Lboot_cpu:
    // Dispatch on the current exception level (bits [3:2] of CurrentEL).
    mrs     x1, CurrentEL
    lsr     x1, x1, #2
    cmp     x1, #2
    b.ne    .Lin_el1
    adr     x20, .Lin_el1
    b       _el2_establish_and_drop

.Lin_el1:
    // SCTLR_EL1 is architecturally UNKNOWN here on real silicon — both
    // behind the EL2->EL1 drop above and on a direct-EL1 load — and an
    // UNKNOWN EE (big-endian data) or WXN bit wrecks EL1 the moment it
    // is exercised (QEMU resets the register benignly, masking this).
    // Establish the known MMU-off value before the first EL1 data
    // access: 0x30D0_0800 = the ARMv8.0 RES1 bits only, i.e.
    // `rustos_arch_aarch64::paging::SCTLR_MMU_OFF` (a unit test there
    // pins this hard-coded value).
    mov     x0, #0x0800
    movk    x0, #0x30D0, lsl #16
    msr     sctlr_el1, x0
    isb

    // Establish the boot stack (top of the linker-reserved region).
    adrp    x0, __boot_stack_top
    add     x0, x0, :lo12:__boot_stack_top
    mov     sp, x0

    // Zero [__bss_start, __bss_end) in 16-byte strides.
    adrp    x0, __bss_start
    add     x0, x0, :lo12:__bss_start
    adrp    x1, __bss_end
    add     x1, x1, :lo12:__bss_end
1:
    cmp     x0, x1
    b.hs    2f
    stp     xzr, xzr, [x0], #16
    b       1b
2:

    // Hand the DTB pointer to the Rust entry. It does not return.
    mov     x0, x19
    bl      rustos_arch_aarch64_main

    // Defensive park (unreachable: the Rust entry never returns).
3:
    wfi
    b       3b

// --- Entered at EL2: establish fully-known EL2 state, drop to EL1 ---
//
// Shared by the boot core's `_start` and the spin-table secondary
// trampoline (`smp.s` `_start_secondary_spintable_aarch64`) — both may
// be entered at EL2 on the Pi firmware hand-off, and the EL2 register
// establishment is banked per core, so every arriving core runs it.
//
// Contract: x20 = EL1 continuation address; clobbers x0 only; `eret`s
// into EL1h at x20 with DAIF masked. Callee-saved state (x19 = the boot
// DTB pointer) is untouched.
//
// Every EL2 control register below is architecturally UNKNOWN at first
// entry on real silicon (the Pi firmware stub sets only SCTLR_EL2 and
// CPUECTLR_EL1.SMPEN); QEMU resets them to benign zeroes, masking any
// residue. Each is therefore *written whole* with its known hand-off
// value (`rustos_arch_aarch64::el2`, unit-test-pinned) — an `orr` into
// the live register would carry UNKNOWN bits (HCR_EL2.TVM traps EL1's
// first MAIR/TCR/TTBR/SCTLR write into vector-less EL2: the silent Pi 4
// MMU-switch hang).
.global _el2_establish_and_drop
_el2_establish_and_drop:
    // HCR_EL2 = el2::HCR_EL2_HANDOFF: EL1 is AArch64 (RW), stage-2
    // translation off, no traps, no TGE.
    mov     x0, #(1 << 31)
    msr     hcr_el2, x0

    // CNTHCTL_EL2 = el2::CNTHCTL_EL2_HANDOFF: EL1/EL0 read the physical
    // counter and program the physical timer without trapping to EL2
    // (EL1PCTEN | EL1PCEN); zero virtual-counter offset.
    mov     x0, #3
    msr     cnthctl_el2, x0
    msr     cntvoff_el2, xzr

    // CPTR_EL2 = el2::CPTR_EL2_HANDOFF: the RES1 bits only — FP/SIMD
    // (TFP) and CPACR_EL1 (TCPAC) accesses from EL1 do not trap.
    mov     x0, #0x33ff
    msr     cptr_el2, x0

    // MDCR_EL2 = el2::MDCR_EL2_HANDOFF: no debug/PMU traps to EL2.
    msr     mdcr_el2, xzr

    // EL1 reads of MIDR_EL1/MPIDR_EL1 return VPIDR_EL2/VMPIDR_EL2:
    // mirror the silicon's own identity registers so EL1 never sees an
    // UNKNOWN core id.
    mrs     x0, midr_el1
    msr     vpidr_el2, x0
    mrs     x0, mpidr_el1
    msr     vmpidr_el2, x0

    // eret into EL1h (M[3:0]=0b0101) with DAIF masked (bits [9:6]).
    mov     x0, #0x3c5
    msr     spsr_el2, x0
    msr     elr_el2, x20
    eret

// Boot stack. Reserved in `.bss` (zeroed by the loop above); 64 KiB is
// generous headroom for the single-CPU boot pipeline.
.section .bss.boot_stack, "aw", %nobits
.balign 16
__boot_stack_bottom:
    .skip 65536
__boot_stack_top:
