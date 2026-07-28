// TAIRiX x86_64 machine-takeover stack-switch trampoline + relocatable
// self-test + reset stub.
//
// `plans/NEW-SUPERVISOR.md` §9 Stage B (x86_64). These are the two small,
// register-only assembly fragments the machine-takeover needs that cannot be
// expressed in Rust: the switch onto the reserved stack, and the final,
// position-independent routine that runs *after* the architecture-neutral
// sweep has destructively tested and overwritten every *usable* frame.
//
// AT&T syntax to match the rest of the x86_64 port (`boot.s`, `context.s`).
//
// # `_takeover_switch_stack`
//
// The destructive sweep must not run on the caller's stack (a
// kernel-service kthread stack allocated from *usable* RAM, which the sweep
// destroys). This trampoline installs the reserved (`.bss`, never-swept)
// takeover stack and tail-calls the Rust continuation on it, so the sweep's
// own frames live in reserved memory. It never returns (the continuation is
// `-> !`).
//
// Entry contract (System V AMD64):
//   %rdi = thin pointer to the caller's `&mut dyn FnMut()` sweep handle
//          (passed straight through to the continuation as its 1st argument)
//   %rsi = top of the reserved stack (16-byte aligned, grows down)
//
// # `_takeover_stub`
//
// After the sweep has tested every *usable* frame, the one region it could
// not touch is the memory it executed from — the kernel image and the stack
// it ran on, the *physical* range `[__boot_phys_start, __kernel_phys_end)`.
// This stub tests that region. It must therefore not execute from it: the
// takeover copies these bytes into a freshly-swept *usable* page above the
// kernel image (the "arena") and jumps to the copy at its identity address.
//
// Long mode requires paging, and the kernel's live page tables sit *inside*
// the region under test, so the stub first switches to a minimal identity
// page table the takeover built in that same arena (its %cr3 physical address
// is passed in %rdx). The arena — the stub's code page and its page tables —
// is the only RAM excluded from every test (exactly memtest86's relocated
// self-test residue); it was already swept once before being repurposed. The
// stub uses **no stack** (the reserved stack it was called on is inside the
// region under test), so it is register-only, and it never touches the low
// firmware/ACPI reserved RAM.
//
// Entry contract (System V AMD64 integer registers, set by the caller's
// indirect jump — not a `call`, so there is no return address and no stack
// use):
//   %rdi = first byte of the region to test (8-byte aligned, = __boot_phys_start)
//   %rsi = one past the last byte        (8-byte aligned, = __kernel_phys_end)
//   %rdx = physical address of the arena identity %cr3 to install
// The routine never returns: it switches %cr3, destructively two-pass tests
// [%rdi, %rsi) with moving-inversions polarity coverage (matching the
// arch-neutral engine's `destructive_window`), then resets the platform
// through the legacy 8042 / `0xCF9` reset hardware (the same channels
// `reset::reboot` drives) and, defensively, parks on `hlt`.
//
// `_takeover_stub_end` bounds the byte length the caller copies; keep it
// immediately after the body with no trailing padding directives.

.section .text, "ax"
.code64
.balign 16
.global _takeover_switch_stack
.type _takeover_switch_stack, @function
_takeover_switch_stack:
    movq    %rsi, %rsp                              // install the reserved stack
    // %rdi already holds the thin sweep-handle pointer (1st SysV argument).
    // `call` (not `jmp`) so the continuation sees the ABI-required 16-byte
    // stack alignment at entry; it is `-> !`, so the return is never taken.
    call    tairix_arch_x86_64_takeover_continue
    // Defensive: the continuation never returns.
1:
    hlt
    jmp     1b
.size _takeover_switch_stack, . - _takeover_switch_stack

.balign 16
.global _takeover_stub
.type _takeover_stub, @function
_takeover_stub:
    // Install the arena identity page table (%rdx). `mov`-to-%cr3 flushes the
    // non-global TLB, so the next fetch (this stub, mapped identity in the
    // arena) and every test access below re-walk the arena table — never the
    // now-abandoned kernel tables inside the region under test.
    movq    %rdx, %cr3

    // Pass 1: fill [%rdi, %rsi) with 0xAAAA_AAAA_AAAA_AAAA, then read it back.
    // A stuck bit or shorted line surfaces on the read; there is nowhere left
    // to report it (the console's RAM is gone), so the coverage is the
    // exercise itself, exactly as memtest86's final self-test region.
    movabsq $0xAAAAAAAAAAAAAAAA, %rax
    movq    %rdi, %rcx
1:
    cmpq    %rsi, %rcx
    jae     2f
    movq    %rax, (%rcx)
    addq    $8, %rcx
    jmp     1b
2:
    movq    %rdi, %rcx
3:
    cmpq    %rsi, %rcx
    jae     4f
    movq    (%rcx), %r8
    addq    $8, %rcx
    jmp     3b
4:
    // Pass 2: the complementary pattern 0x5555_5555_5555_5555, proving the
    // opposite polarity of every bit.
    movabsq $0x5555555555555555, %rax
    movq    %rdi, %rcx
5:
    cmpq    %rsi, %rcx
    jae     6f
    movq    %rax, (%rcx)
    addq    $8, %rcx
    jmp     5b
6:
    movq    %rdi, %rcx
7:
    cmpq    %rsi, %rcx
    jae     8f
    movq    (%rcx), %r8
    addq    $8, %rcx
    jmp     7b
8:
    // Reset. x86 has no architected reset instruction; drive the legacy PC
    // reset hardware every PC-class chipset and the QEMU pc/q35 machines
    // decode, the same channels `reset::reboot` uses: the 8042 "pulse output
    // line" reset first, then the `0xCF9` reset-control register (arm SYS_RST,
    // then request the full CPU reset).
    movw    $0x64, %dx
    movb    $0xFE, %al
    outb    %al, %dx
    movw    $0xCF9, %dx
    movb    $0x02, %al
    outb    %al, %dx
    movb    $0x0E, %al
    outb    %al, %dx
9:
    // Defensive park: unreachable once the platform resets, but a core must
    // never fall through into arbitrary bytes.
    hlt
    jmp     9b
.global _takeover_stub_end
_takeover_stub_end:
