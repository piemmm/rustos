# TAIRiX riscv64 boot trampoline for the QEMU `virt` board.
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
#   4. `tairix_arch_riscv64_main` is `-> !` and never returns; the
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

    # Poison the guard the linker reserved below the stack, so a later
    # overrun is a disturbed sentinel the panic path can name rather than
    # a silent clobber of the statics under it. After the loop above,
    # which spans the guard and would otherwise erase it. `a0`/`a1` carry
    # the SBI hand-off and are left alone.
    la      t0, __boot_stack_guard_bottom
    la      t1, __boot_stack_bottom
    li      t2, {GUARD_BYTE}
.Lpoison_guard:
    bgeu    t0, t1, .Lpoison_guard_done
    sb      t2, 0(t0)
    addi    t0, t0, 1
    j       .Lpoison_guard
.Lpoison_guard_done:

    # Record the boot hartid in `tp` so `smp::current_hartid` recovers
    # this hart's identity from a per-CPU register, exactly as the
    # secondary stub (`smp.s`) does for every other hart. `a0` still
    # holds the SBI-handed hartid here (the bss loop touched only
    # `t0`/`t1`).
    mv      tp, a0

    # Hand (hartid, dtb) to the Rust entry. `a0`/`a1` already hold the
    # SBI hand-off values, so they pass straight through. It does not
    # return.
    call    tairix_arch_riscv64_main

    # Defensive park (unreachable: the Rust entry never returns).
3:
    wfi
    j       3b

# The boot stack is reserved by the active linker script, which sizes it
# per image (`BOOT_STACK_BYTES`) and places it last in `.bss` so an
# overrun cannot reach `.rodata`/`.text`. A size fixed here instead would
# be one constant for every image, and a workload heavier than the boot
# pipeline silently outgrew it.
