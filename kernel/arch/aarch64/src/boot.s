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
// loop here, before touching the single boot stack or `.bss`, and waits
// for the SMP bring-up (`plans/PI.md` P5) to start it deliberately. This
// fails closed (`AGENTS.md` §2.1 — never race the secondaries onto a
// shared stack).
//
// SAFETY-INVARIANTs (audited per AGENTS.md §10):
//   1. The body below `_start` runs exactly once, on the boot CPU
//      (affinity 0); every other CPU is trapped in the `wfe` park above
//      it. Interrupts are masked before any handler exists.
//   2. x0 carries the DTB pointer described above; it is preserved across
//      the EL2->EL1 drop in callee-saved x19 and restored before the call.
//   3. The stack top and `.bss` bounds come from the active linker script
//      (`aarch64-virt.ld` / `aarch64-rpi4.ld`); `__bss_start`/`__bss_end`
//      are 16-byte aligned so the `stp` clear loop is exact.
//   4. `rustos_arch_aarch64_main` is `-> !` and never returns; the trailing
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
    // loop until the SMP bring-up starts it explicitly (plans/PI.md P5).
    // This runs before the shared boot stack or `.bss` is touched, so a
    // released-at-reset Pi secondary cannot race the boot CPU. Scratch
    // registers x4..x7 are used so x19 (the DTB) is left untouched.
    mrs     x4, mpidr_el1
    mov     x5, #0xffff             // Aff0 [7:0] | Aff1 [15:8]
    movk    x5, #0xff, lsl #16      // | Aff2 [23:16]  => x5 = 0x00FF_FFFF
    and     x6, x4, x5
    ubfx    x7, x4, #32, #8         // Aff3 [39:32]
    orr     x6, x6, x7
    cbz     x6, .Lboot_cpu
.Lpark_secondary:
    wfe
    b       .Lpark_secondary

.Lboot_cpu:
    // Dispatch on the current exception level (bits [3:2] of CurrentEL).
    mrs     x1, CurrentEL
    lsr     x1, x1, #2
    cmp     x1, #2
    b.ne    .Lin_el1

    // --- Entered at EL2: configure and drop to EL1 ---
    // EL1 executes in AArch64 (HCR_EL2.RW = 1).
    mrs     x0, hcr_el2
    orr     x0, x0, #(1 << 31)
    msr     hcr_el2, x0

    // Let EL1/EL0 read the physical counter and program the physical
    // timer without trapping to EL2 (CNTHCTL_EL2.EL1PCTEN | EL1PCEN),
    // and present a zero virtual-counter offset.
    mrs     x0, cnthctl_el2
    orr     x0, x0, #3
    msr     cnthctl_el2, x0
    msr     cntvoff_el2, xzr

    // eret into EL1h (M[3:0]=0b0101) with DAIF masked (bits [9:6]).
    mov     x0, #0x3c5
    msr     spsr_el2, x0
    adr     x0, .Lin_el1
    msr     elr_el2, x0
    eret

.Lin_el1:
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

// Boot stack. Reserved in `.bss` (zeroed by the loop above); 64 KiB is
// generous headroom for the single-CPU boot pipeline.
.section .bss.boot_stack, "aw", %nobits
.balign 16
__boot_stack_bottom:
    .skip 65536
__boot_stack_top:
