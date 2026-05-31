// RustOS aarch64 boot trampoline for the QEMU `virt` board.
//
// QEMU (`-kernel <elf>`) loads this ELF at its physical link address and
// enters `_start` with the Linux aarch64 boot-protocol hand-off:
//
//   x0 = physical address of the flattened device tree (DTB)
//   x1..x3 = 0 (reserved)
//
// On the default `virt` machine (no EL3, EL2 only when `virtualization=on`)
// the highest implemented exception level is EL1, but a `virtualization=on`
// board enters at EL2. This stub handles both: if entered at EL2 it
// configures EL1 to run AArch64, grants EL1/EL0 the physical counter/timer,
// zeroes the virtual counter offset, and `eret`s to EL1; if already at EL1
// it proceeds directly. It then establishes a stack, zeroes `.bss` so Rust
// statics start cleared, and tail-calls the Rust entry with the DTB pointer
// preserved in x0.
//
// SAFETY-INVARIANTs (audited per AGENTS.md §10):
//   1. Entered exactly once, on the boot CPU, with interrupts to be masked
//      here before any handler exists.
//   2. x0 carries the DTB pointer described above; it is preserved across
//      the EL2->EL1 drop in callee-saved x19 and restored before the call.
//   3. The stack top and `.bss` bounds come from the linker script
//      (`aarch64-virt.ld`); `__bss_start`/`__bss_end` are 16-byte aligned
//      so the `stp` clear loop is exact.
//   4. `rustos_arch_aarch64_main` is `-> !` and never returns; the trailing
//      `wfi` park is unreachable under QEMU but is the correct conservative
//      behaviour on bare metal.

.section .text.boot, "ax"
.global _start
_start:
    // Mask all interrupts (D, A, I, F) during bring-up.
    msr     DAIFSet, #0xf

    // Preserve the DTB pointer across the EL setup (callee-saved).
    mov     x19, x0

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
