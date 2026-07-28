// TAIRiX aarch64 machine-takeover relocatable self-test + reset stub.
//
// `plans/NEW-SUPERVISOR.md` §9 Stage B (aarch64). This is the small,
// position-independent routine the machine-takeover runs as its *final*
// phase, after the architecture-neutral sweep has destructively tested and
// overwritten every *usable* frame. Its job is the one region the sweep
// could not touch: the memory the sweep itself executed from — the kernel
// image and the stack it ran on, `[__kernel_start, __kernel_end)`.
//
// It must therefore not execute from that region: the takeover copies these
// bytes into a freshly-tested *usable* page (identity-addressed and
// executable under the MMU-off regime the takeover installed) and jumps to
// the copy. The routine is fully position-independent — it references no
// kernel symbol and forms no absolute address into the region it is about to
// destroy — and uses **no stack** (the reserved stack it was called on also
// lies inside the region under test), so it is register-only.
//
// It never touches the firmware / DTB / low reserved RAM below the kernel
// image: the caller passes only the kernel-image bounds, so that region is
// excluded by construction and the PSCI reset conduit (which lives there)
// still works.
//
// Entry contract (AAPCS64 integer registers):
//   x0 = first byte of the region to test  (8-byte aligned)
//   x1 = one past the last byte            (8-byte aligned, x1 >= x0)
//   x2 = reset conduit: 0 = `hvc #0` (PSCI at EL2), 1 = `smc #0` (EL3)
// The routine never returns: it destructively tests [x0, x1) with a
// two-pass moving-inversions sweep (matching the arch-neutral engine's
// `destructive_window` polarity coverage), then issues PSCI `SYSTEM_RESET`
// through the conduit and, defensively, parks on `wfi`.
//
// `_takeover_stub_end` bounds the byte length the caller copies; keep it
// immediately after the body with no trailing padding directives.

// Switch onto the reserved takeover stack and tail-call the continuation.
//
// The destructive sweep must not run on a stack the sweep itself destroys.
// This trampoline installs the reserved (`.bss`, never-swept) stack the
// takeover reserves and tail-calls the Rust continuation on it, so the
// sweep's own frames live in reserved memory. It never returns (the
// continuation is `-> !`).
//
// Entry contract (AAPCS64 integer registers):
//   x0 = thin pointer to the caller's `&mut dyn FnMut()` sweep handle
//        (passed straight through to the continuation)
//   x1 = top of the reserved stack (16-byte aligned, grows down)
.section .text, "ax"
.balign 4
.global _takeover_switch_stack
_takeover_switch_stack:
    mov     sp, x1
    b       tairix_arch_aarch64_takeover_continue

.balign 4
.global _takeover_stub
_takeover_stub:
    // Pass 1: fill [x0, x1) with the pattern 0xAAAA_AAAA_AAAA_AAAA, then
    // verify. A stuck-low bit or a shorted address line surfaces on the
    // read-back; there is nowhere left to report it (the console's RAM is
    // gone), so a mismatch simply falls through to the reset — the coverage
    // is the exercise itself, exactly as memtest86's final self-test region.
    movz    x9, #0xAAAA
    movk    x9, #0xAAAA, lsl #16
    movk    x9, #0xAAAA, lsl #32
    movk    x9, #0xAAAA, lsl #48
    mov     x10, x0
1:
    cmp     x10, x1
    b.hs    2f
    str     x9, [x10]
    add     x10, x10, #8
    b       1b
2:
    mov     x10, x0
3:
    cmp     x10, x1
    b.hs    4f
    ldr     x11, [x10]
    add     x10, x10, #8
    b       3b
4:
    // Pass 2: the complementary pattern 0x5555_5555_5555_5555, proving the
    // opposite polarity of every bit.
    movz    x9, #0x5555
    movk    x9, #0x5555, lsl #16
    movk    x9, #0x5555, lsl #32
    movk    x9, #0x5555, lsl #48
    mov     x10, x0
5:
    cmp     x10, x1
    b.hs    6f
    str     x9, [x10]
    add     x10, x10, #8
    b       5b
6:
    mov     x10, x0
7:
    cmp     x10, x1
    b.hs    8f
    ldr     x11, [x10]
    add     x10, x10, #8
    b       7b
8:
    // PSCI SYSTEM_RESET (SMC32 fast call, service 0, function 9 =
    // 0x8400_0009): resets the whole system, reason "none", and never
    // returns. The conduit selector in x2 chooses the trap instruction the
    // firmware answers on (`hvc` at EL2, `smc` at EL3).
    movz    w0, #0x0009
    movk    w0, #0x8400, lsl #16
    cbnz    x2, 9f
    hvc     #0
    b       10f
9:
    smc     #0
10:
    // Defensive park: unreachable once the firmware resets the platform,
    // but a core must never fall through into arbitrary bytes.
    wfi
    b       10b
.global _takeover_stub_end
_takeover_stub_end:
