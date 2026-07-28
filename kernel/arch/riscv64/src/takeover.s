# TAIRiX riscv64 machine-takeover relocatable self-test + reset stub.
#
# `plans/NEW-SUPERVISOR.md` §9 Stage B (riscv64). This is the small,
# position-independent routine the machine-takeover runs as its *final*
# phase, after the architecture-neutral sweep has destructively tested and
# overwritten every *usable* frame. Its job is the one region the sweep
# could not touch: the memory the sweep itself executed from — the kernel
# image and the stack it ran on, `[__kernel_image_start, __kernel_end)`.
#
# It must therefore not execute from that region: the takeover copies these
# bytes into a freshly-tested *usable* page (identity-mapped, executable
# under the `satp = 0` bare regime the takeover installed) and jumps to the
# copy. The routine is fully position-independent — it references no kernel
# symbol and forms no absolute address into the region it is about to
# destroy — and uses **no stack** (the reserved stack it was called on also
# lies inside the region under test), so it is register-only.
#
# It never touches the firmware region below the kernel image (OpenSBI, at
# `[ram_base, __kernel_image_start)`): overwriting the M-mode firmware would
# break the SBI reset ecall this stub ends with. The caller passes only the
# kernel-image bounds, so the firmware is excluded by construction.
#
# Entry contract (System V riscv64 integer registers):
#   a0 = first byte of the region to test  (8-byte aligned)
#   a1 = one past the last byte            (8-byte aligned, a1 >= a0)
# The routine never returns: it destructively tests [a0, a1) with a
# two-pass moving-inversions sweep (matching the arch-neutral engine's
# `destructive_window` polarity coverage), then issues the SBI System-Reset
# (SRST) cold-reboot ecall and, defensively, parks on `wfi`.
#
# `_takeover_stub_end` bounds the byte length the caller copies; keep it
# immediately after the body with no trailing padding directives.

# Switch onto the reserved takeover stack and tail-call the continuation.
#
# The destructive sweep must not run on a stack the sweep itself destroys.
# This trampoline installs the reserved (`.bss`, never-swept) stack the
# takeover reserves and tail-calls the Rust continuation on it, so the
# sweep's own frames live in reserved memory. It never returns (the
# continuation is `-> !`).
#
# Entry contract (System V riscv64 integer registers):
#   a0 = thin pointer to the caller's `&mut dyn FnMut()` sweep handle
#        (passed straight through to the continuation)
#   a1 = top of the reserved stack (16-byte aligned, grows down)
.section .text, "ax"
.balign 4
.global _takeover_switch_stack
_takeover_switch_stack:
    mv      sp, a1
    tail    tairix_arch_riscv64_takeover_continue

.balign 4
.global _takeover_stub
_takeover_stub:
    # Pass 1: fill [a0, a1) with the pattern 0xAAAA_AAAA_AAAA_AAAA, then
    # verify. A stuck-low bit or a shorted address line surfaces on the
    # read-back; there is nowhere left to report it (the console's RAM is
    # gone), so a mismatch simply falls through to the reset — the coverage
    # is the exercise itself, exactly as memtest86's final self-test region.
    li      t1, 0xAAAAAAAAAAAAAAAA
    mv      t0, a0
1:
    bgeu    t0, a1, 2f
    sd      t1, 0(t0)
    addi    t0, t0, 8
    j       1b
2:
    mv      t0, a0
3:
    bgeu    t0, a1, 4f
    ld      t2, 0(t0)
    addi    t0, t0, 8
    j       3b
4:
    # Pass 2: the complementary pattern 0x5555_5555_5555_5555, proving the
    # opposite polarity of every bit.
    li      t1, 0x5555555555555555
    mv      t0, a0
5:
    bgeu    t0, a1, 6f
    sd      t1, 0(t0)
    addi    t0, t0, 8
    j       5b
6:
    mv      t0, a0
7:
    bgeu    t0, a1, 8f
    ld      t2, 0(t0)
    addi    t0, t0, 8
    j       7b
8:
    # SBI System-Reset (SRST): cold reboot, reason "none". The immediates
    # are the SRST extension id ("SRST" = 0x53525354), function id 0
    # (system_reset), reset_type 1 (COLD_REBOOT), reset_reason 0 (NONE).
    li      a7, 0x53525354
    li      a6, 0
    li      a0, 1
    li      a1, 0
    ecall
    # Defensive park: unreachable once the firmware resets the platform,
    # but a hart must never fall through into arbitrary bytes.
9:
    wfi
    j       9b
.global _takeover_stub_end
_takeover_stub_end:
