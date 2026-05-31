# RustOS riscv64 boot trampoline for the QEMU `virt` board.
#
# OpenSBI (loaded by `-bios default`) runs in M-mode, then enters this
# ELF's entry point in S-mode with paging disabled (`satp = 0`, bare
# addressing) and the hand-off registers set by the SBI boot protocol:
#
#   a0 = hartid of the boot hart
#   a1 = physical address of the flattened device tree (DTB)
#
# This stub is the only assembly in the riscv64 port: it establishes a
# stack, zeroes the `.bss` so Rust statics start cleared (required on
# real hardware; QEMU's ELF loader also zero-fills, so this is
# defence-in-depth), and tail-calls the Rust entry. The 64 MiB boot
# heap lives in its own `.heap` (NOLOAD) section *outside* the zeroed
# range — the bump allocator does not require zeroed backing — so the
# memset stays cheap.
#
# SAFETY-INVARIANTs:
#   1. Entered exactly once, on the boot hart, in S-mode, paging off.
#   2. `a0`/`a1` carry the SBI hand-off values described above.
#   3. The default code model is `medany`: every reference is
#      pc-relative, so no global pointer (`gp`) setup is required.
#   4. `rustos_arch_riscv64_main` is `-> !` and never returns; the
#      trailing `wfi` park is unreachable under QEMU but is the correct
#      conservative behaviour on bare metal.

.section .text.boot, "ax"
.global _start
_start:
    # Establish the boot stack (grows down from the top of the reserved
    # region). No frame has been pushed yet, so zeroing the stack region
    # below `sp` in the loop that follows is safe.
    la      sp, __boot_stack_top

    # Zero [__bss_start, __bss_end). Both bounds are 8-byte aligned by
    # the linker script, so the doubleword store loop is exact.
    la      t0, __bss_start
    la      t1, __bss_end
1:
    bgeu    t0, t1, 2f
    sd      zero, 0(t0)
    addi    t0, t0, 8
    j       1b
2:

    # Record the boot hartid in `tp` so `smp::current_hartid` recovers
    # this hart's identity from a per-CPU register, exactly as the
    # secondary stub (`smp.s`) does for every other hart. `a0` still
    # holds the SBI-handed hartid here (the bss loop touched only
    # `t0`/`t1`).
    mv      tp, a0

    # Hand (hartid, dtb) to the Rust entry. `a0`/`a1` already hold the
    # SBI hand-off values, so they pass straight through. It does not
    # return.
    call    rustos_arch_riscv64_main

    # Defensive park (unreachable: the Rust entry never returns).
3:
    wfi
    j       3b

# Boot stack. Lives in `.bss` so it is zeroed by the loop above; 64 KiB
# is generous headroom for the single-hart boot pipeline.
.section .bss.boot_stack, "aw", @nobits
.balign 16
__boot_stack_bottom:
    .skip 65536
__boot_stack_top:
