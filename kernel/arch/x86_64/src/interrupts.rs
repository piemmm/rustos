//! x86_64 IDT builder and common ISR prologue (Stage 3a (c2)).
//!
//! This module owns the architecturally-fixed surface every interrupt
//! and exception delivery on x86_64 needs:
//!
//!   * [`InterruptStackFrame`] — the five 8-byte words the CPU itself
//!     pushes on every interrupt entry in long mode (Intel SDM Vol 3A
//!     §6.14.2 Figure 6-8): `rip`, `cs`, `rflags`, `rsp`, `ss`.
//!   * [`SavedRegs`] — the 15 general-purpose registers the common
//!     prologue persists *immediately* on top of the CPU-pushed frame
//!     so a Rust handler can inspect or mutate them through a
//!     `&mut SavedRegs`.
//!   * [`IdtEntry`] / [`Idt`] / `IdtPointer` — the descriptor table
//!     itself, addressed by `lidt`.
//!   * `Idt::load` — the bare-metal install routine (gated to
//!     `target_os = "none"`).
//!
//! # Common ISR prologue
//!
//! The assembly half lives in `interrupts.s` and exports the symbol
//! `rustos_arch_x86_64_isr_default`. Every IDT vector populated by
//! [`Idt::with_default_handler`] routes through this single thunk. On
//! entry the prologue:
//!
//!   1. Pushes `0` as a synthetic "error code" if the CPU did not push
//!      one (vectors that *do* push a hardware error code — 8, 10–14,
//!      17, 21 — would need a vector-specific stub; the default
//!      thunk treats every vector as no-error, which is correct for the
//!      Stage 3a (c1/c2/c3) scope because no IDT slot is wired to a
//!      hardware-error vector here. The Stage 3a (c5) preemption commit
//!      that wires real ISRs will extend `define_isr!` to emit
//!      vector-specific stubs.)
//!   2. Pushes the full 15-GPR [`SavedRegs`] block in the layout the
//!      `repr(C)` definition pins below.
//!   3. Loads `rdi` with a pointer to the saved-regs block and calls
//!      `rustos_arch_x86_64_default_interrupt`, which is `-> !` and
//!      therefore must terminate the kernel through `qemu_exit` or the
//!      equivalent platform-specific failure path.
//!
//! The "must terminate" property is intentional: the only consumers
//! of the default thunk today are *unexpected* interrupts that
//! `AGENTS.md` §10 requires to fail closed. A real ISR (LAPIC timer,
//! syscall entry, etc.) lives behind its own dedicated thunk emitted
//! by the (c5) follow-up; it is *not* shoe-horned into the default
//! path. This avoids the §15.5 anti-pattern of a "convenience wrapper"
//! that ends up carrying production logic.

use core::mem::size_of;

// --- CPU-pushed interrupt stack frame ------------------------------

/// The five 8-byte words the CPU pushes on every long-mode interrupt /
/// exception delivery (Intel SDM Vol 3A §6.14.2 Figure 6-8).
///
/// Order (ascending address from %rsp at the moment a Rust handler
/// is entered with the prologue's `SavedRegs` already on top):
///
/// | Offset | Field    |
/// |--------|----------|
/// | +0x00  | `rip`    |
/// | +0x08  | `cs`     |
/// | +0x10  | `rflags` |
/// | +0x18  | `rsp`    |
/// | +0x20  | `ss`     |
///
/// `cs` and `ss` are 64-bit on x86_64 even though only the low 16 bits
/// carry the selector — the CPU sign-extends and pushes the full
/// qword. Decoding the actual selector is `frame.cs as u16`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterruptStackFrame {
    /// Faulting / returning instruction pointer.
    pub rip: u64,
    /// Code selector at the moment of the interrupt (full 64-bit push).
    pub cs: u64,
    /// Saved `RFLAGS`.
    pub rflags: u64,
    /// Saved stack pointer (matters for ring-3 → ring-0 entries; equal
    /// to the inbound %rsp for ring-0 → ring-0 entries that did not
    /// switch via an IST).
    pub rsp: u64,
    /// Stack-segment selector (full 64-bit push).
    pub ss: u64,
}

// --- Saved GPR block ------------------------------------------------

/// The 15 general-purpose registers the common ISR prologue persists
/// immediately on top of the [`InterruptStackFrame`].
///
/// Order is **push-descending** — the prologue executes
/// `pushq %rax; pushq %rcx; …; pushq %r15` in that order, so on entry
/// to the Rust handler the lowest address (smallest %rsp offset) holds
/// `r15` and the highest holds `rax`. The `repr(C)` field order below
/// reflects that ascending-address layout.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavedRegs {
    /// Saved `r15`.
    pub r15: u64,
    /// Saved `r14`.
    pub r14: u64,
    /// Saved `r13`.
    pub r13: u64,
    /// Saved `r12`.
    pub r12: u64,
    /// Saved `r11`.
    pub r11: u64,
    /// Saved `r10`.
    pub r10: u64,
    /// Saved `r9`.
    pub r9: u64,
    /// Saved `r8`.
    pub r8: u64,
    /// Saved `rdi`.
    pub rdi: u64,
    /// Saved `rsi`.
    pub rsi: u64,
    /// Saved `rbp`.
    pub rbp: u64,
    /// Saved `rbx`.
    pub rbx: u64,
    /// Saved `rdx`.
    pub rdx: u64,
    /// Saved `rcx`.
    pub rcx: u64,
    /// Saved `rax`.
    pub rax: u64,
}

/// Layout-pinning const-assertions used by the assembly in
/// `interrupts.s`. Never referenced at runtime; serve as the failure
/// boundary if anyone ever reorders a field.
#[allow(dead_code)] // const-asserts.
const ISR_LAYOUT_PINS: () = {
    assert!(size_of::<SavedRegs>() == 15 * 8);
    assert!(size_of::<InterruptStackFrame>() == 5 * 8);
};

// --- IDT entry ------------------------------------------------------

/// One 16-byte IDT slot in long mode (Intel SDM Vol 3A §6.14.1
/// Figure 6-7).
///
/// The fields are stored exactly in their on-wire order so the table
/// can be addressed by `lidt` without any per-entry conversion. Build
/// entries via [`IdtEntry::interrupt_gate`]; the raw constructor is
/// reserved for the const-init that backs [`Idt::empty`].
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct IdtEntry {
    /// Bits 0..16 of the handler offset.
    pub offset_lo: u16,
    /// Code segment selector that will be loaded into `cs` on entry.
    pub selector: u16,
    /// `IST` index (0..=7). A value of 0 means "use the privilege-
    /// stack selected by the existing CPU state"; 1..=7 selects an
    /// IST stack from the active TSS.
    pub ist: u8,
    /// Type and attributes byte: `P[7] | DPL[6:5] | 0 | type[3:0]`.
    /// For an interrupt gate `type = 0xE`; for a trap gate `0xF`.
    pub type_attr: u8,
    /// Bits 16..32 of the handler offset.
    pub offset_mid: u16,
    /// Bits 32..64 of the handler offset.
    pub offset_hi: u32,
    /// Reserved; must be zero per the SDM.
    pub zero: u32,
}

/// Type-attr value for a 64-bit interrupt gate with `DPL = 0, P = 1`.
pub const TYPE_ATTR_KERNEL_INTERRUPT: u8 = 0x8E;

impl IdtEntry {
    /// All-zero entry. Identical to "Present = 0", which the CPU
    /// treats as an absent vector that raises `#NP` on delivery — an
    /// acceptable fail-closed default until [`Idt::with_default_handler`]
    /// runs.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            offset_lo: 0,
            selector: 0,
            ist: 0,
            type_attr: 0,
            offset_mid: 0,
            offset_hi: 0,
            zero: 0,
        }
    }

    /// Build a 64-bit interrupt gate (type = 0xE) at DPL 0.
    ///
    /// `handler` is the linear address of the ISR entry point.
    /// `selector` is the code-segment selector loaded into `cs` on
    /// entry (normally `kernel_cs` from the per-CPU GDT). `ist` is the
    /// 0..=7 IST index; 0 disables the IST swap.
    ///
    /// # Panics
    ///
    /// Never — this routine validates `ist <= 7` by masking and
    /// preserves the documented behaviour for any other input.
    #[must_use]
    pub const fn interrupt_gate(handler: u64, selector: u16, ist: u8) -> Self {
        Self {
            offset_lo: (handler & 0xFFFF) as u16,
            selector,
            ist: ist & 0x7,
            type_attr: TYPE_ATTR_KERNEL_INTERRUPT,
            offset_mid: ((handler >> 16) & 0xFFFF) as u16,
            offset_hi: ((handler >> 32) & 0xFFFF_FFFF) as u32,
            zero: 0,
        }
    }

    /// Reconstruct the full 64-bit handler offset from the three
    /// split fields. Test-only — production code never inspects an IDT
    /// entry once it has been installed.
    #[must_use]
    pub const fn handler(self) -> u64 {
        (self.offset_lo as u64) | ((self.offset_mid as u64) << 16) | ((self.offset_hi as u64) << 32)
    }
}

// --- IDT and pointer ------------------------------------------------

/// Number of architecturally-fixed vectors.
pub const IDT_LEN: usize = 256;

/// A full 256-entry IDT.
///
/// Stored as an array of [`IdtEntry`]; the only operations are
/// [`Idt::empty`] (const-init) and [`Idt::with_default_handler`]
/// (populate every slot with a single fail-closed thunk). Further
/// per-vector wiring is the (c5) commit's job.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Idt {
    /// One slot per architecturally-fixed vector.
    pub entries: [IdtEntry; IDT_LEN],
}

impl Idt {
    /// All-empty IDT. Every vector is `P = 0`, which delivers `#NP` on
    /// access. Suitable as a `static` initialiser before
    /// [`Self::with_default_handler`] runs.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            entries: [IdtEntry::empty(); IDT_LEN],
        }
    }

    /// Populate every vector with a 64-bit interrupt gate pointing at
    /// `handler`, using `selector` as the kernel CS. `ist` selects the
    /// IST index for vectors 0..=31 (the architecturally-defined
    /// exceptions); user vectors 32..=255 always use IST 0 because
    /// they are not architecturally permitted to stack-corrupt the
    /// kernel.
    ///
    /// The split mirrors the Intel SDM Vol 3A §6.14.5 guidance:
    /// `#DF` (vector 8) and `#NMI` (vector 2) *must* run on an IST or
    /// the kernel cannot be reasoned about; other exceptions (`#GP`,
    /// `#PF`, etc.) are fine on the per-task stack as long as the
    /// kernel keeps its stack mapped.
    ///
    /// `ist_for_exception` is consulted for each `vector` in `0..=31`
    /// and returns the IST index to write into that slot (0 disables).
    pub fn with_default_handler<F>(handler: u64, selector: u16, mut ist_for_exception: F) -> Self
    where
        F: FnMut(u8) -> u8,
    {
        let mut idt = Self::empty();
        let mut v: usize = 0;
        while v < IDT_LEN {
            // `v` is in `0..IDT_LEN` and `IDT_LEN == 256`, so the cast
            // to `u8` is exact. We could `try_from` here but that
            // pulls a panicking path into a `while`-loop populator;
            // the documented bound makes the truncation impossible.
            #[allow(clippy::cast_possible_truncation)] // 0..=255 fits.
            let v_u8 = v as u8;
            let ist = if v < 32 { ist_for_exception(v_u8) } else { 0 };
            idt.entries[v] = IdtEntry::interrupt_gate(handler, selector, ist);
            v += 1;
        }
        idt
    }

    /// Install this IDT on the current CPU via `lidt`.
    ///
    /// # Safety
    ///
    /// * The `Idt` storage must outlive every interrupt subsequently
    ///   delivered to this CPU — i.e. the caller must store it in
    ///   `'static`-lifetime memory.
    /// * The handlers referenced by the entries must be valid Rust /
    ///   assembly code at their recorded addresses for the duration
    ///   the IDT is active.
    /// * Interrupts must be disabled (`CLI`) at the call site or the
    ///   caller must accept that the *previous* IDT may field a
    ///   delivery that races with `lidt`.
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    pub unsafe fn load(&'static self) {
        let ptr = IdtPointer {
            limit: (size_of::<[IdtEntry; IDT_LEN]>() - 1) as u16,
            base: core::ptr::addr_of!(self.entries) as u64,
        };
        // SAFETY: `ptr` lives on the local stack only for the duration
        // of the `lidt` instruction, which reads it once. The `Idt`
        // backing storage is `'static` per the function's documented
        // safety contract, so the CPU's subsequent dereferences of the
        // recorded base remain valid.
        unsafe {
            core::arch::asm!(
                "lidt [{p}]",
                p = in(reg) &ptr,
                options(readonly, nostack, preserves_flags),
            );
        }
    }
}

/// 10-byte pseudo-descriptor consumed by `lidt`.
#[cfg(any(test, all(target_arch = "x86_64", target_os = "none")))]
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct IdtPointer {
    /// Byte length of the IDT minus one.
    pub limit: u16,
    /// Linear base address of the IDT.
    pub base: u64,
}

// --- define_isr! macro --------------------------------------------

/// Emit a vector-specific ISR stub.
///
/// `define_isr!(my_handler => my_rust_dispatch)` produces a
/// `pub unsafe extern "C" fn my_handler()` that the IDT can address
/// directly. On entry the stub:
///
/// 1. Saves the 15 architectural GPRs in the exact order pinned by
///    [`SavedRegs`] (the same order the default thunk in
///    `interrupts.s` uses; the layout const-assertions in this module
///    are the cross-check).
/// 2. Loads `%rdi` with a pointer to the saved-regs block (i.e. the
///    current `%rsp`).
/// 3. Subtracts 8 from `%rsp` to satisfy the System V AMD64 `call`
///    alignment rule (`%rsp ≡ 8 (mod 16)` at the `call` instruction)
///    so the dispatcher is entered on a 16-byte-aligned stack.
/// 4. Calls `my_rust_dispatch(*mut SavedRegs)`. The dispatcher must
///    be `unsafe extern "C" fn(*mut SavedRegs)`.
/// 5. Restores the GPRs in reverse order and `iretq`s.
///
/// The macro is the only sanctioned way to produce a per-vector stub
/// (`AGENTS.md` §2.5 — no convenience wrappers, §15.5 — no shoe-
/// horning new logic into the fail-closed default thunk). Vectors
/// that push a hardware error code (`8`, `10`–`14`, `17`, `21`) are
/// *not* supported by this macro — they would need an additional
/// stack adjustment after the GPR pushes; emit a dedicated stub if
/// you need to handle one.
///
/// # Why a macro and not a `#[naked]` template fn
///
/// A `#[naked]` template would still need to monomorphise the
/// dispatcher symbol *inside* the assembly (via `sym`), which only
/// works through a macro because const-generic symbols are unstable.
/// The macro form keeps the per-vector stub self-contained, lets
/// `cargo expand` reveal the exact bytes produced, and prevents the
/// `sym` argument from accidentally being templated through a
/// generic function.
#[macro_export]
macro_rules! define_isr {
    ($name:ident => $dispatch:path) => {
        /// Auto-generated vector-specific ISR stub. The body is fully
        /// described in [`crate::define_isr`]'s rustdoc.
        ///
        /// # Safety
        ///
        /// Only the CPU's IDT may invoke this symbol. Calling it
        /// directly from Rust is undefined behaviour because the
        /// function expects an [`crate::interrupts::InterruptStackFrame`]
        /// on top of the stack, not a return address.
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        #[unsafe(naked)]
        #[no_mangle]
        pub unsafe extern "C" fn $name() {
            core::arch::naked_asm!(
                "pushq %rax",
                "pushq %rcx",
                "pushq %rdx",
                "pushq %rbx",
                "pushq %rbp",
                "pushq %rsi",
                "pushq %rdi",
                "pushq %r8",
                "pushq %r9",
                "pushq %r10",
                "pushq %r11",
                "pushq %r12",
                "pushq %r13",
                "pushq %r14",
                "pushq %r15",
                "movq %rsp, %rdi",
                "subq $8, %rsp",
                "call {dispatch}",
                "addq $8, %rsp",
                "popq %r15",
                "popq %r14",
                "popq %r13",
                "popq %r12",
                "popq %r11",
                "popq %r10",
                "popq %r9",
                "popq %r8",
                "popq %rdi",
                "popq %rsi",
                "popq %rbp",
                "popq %rbx",
                "popq %rdx",
                "popq %rcx",
                "popq %rax",
                "iretq",
                dispatch = sym $dispatch,
                options(att_syntax),
            )
        }
    };
}

// --- Default ISR Rust callback -------------------------------------

/// Rust callback invoked by the assembly thunk
/// `rustos_arch_x86_64_isr_default` (`interrupts.s`) with a pointer to
/// the saved-regs block. Treated by `AGENTS.md` §10 as a closed-fail:
/// any unexpected interrupt halts the binary through the platform's
/// QEMU-exit hook.
///
/// Only compiled on the freestanding target — the `qemu_exit` failure
/// helper itself is `cfg(target_os = "none")` (it executes a privileged
/// port-I/O instruction). Host unit tests cover the IDT-entry surface
/// directly; the asm-driven dispatch is exercised end-to-end by the
/// QEMU integration test (`scheduler_stress_qemu`).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[no_mangle]
extern "C" fn rustos_arch_x86_64_default_interrupt(_saved_regs: *mut SavedRegs) -> ! {
    crate::qemu_exit::exit_failure();
}

// --- Tests ----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;

    #[test]
    fn idt_entry_layout_is_pinned() {
        assert_eq!(size_of::<IdtEntry>(), 16);
        assert_eq!(offset_of!(IdtEntry, offset_lo), 0);
        assert_eq!(offset_of!(IdtEntry, selector), 2);
        assert_eq!(offset_of!(IdtEntry, ist), 4);
        assert_eq!(offset_of!(IdtEntry, type_attr), 5);
        assert_eq!(offset_of!(IdtEntry, offset_mid), 6);
        assert_eq!(offset_of!(IdtEntry, offset_hi), 8);
        assert_eq!(offset_of!(IdtEntry, zero), 12);
    }

    #[test]
    fn idt_total_size_matches_lidt_limit() {
        assert_eq!(size_of::<Idt>(), 16 * IDT_LEN);
        assert_eq!(IDT_LEN, 256);
    }

    #[test]
    fn interrupt_gate_splits_handler_address() {
        let handler: u64 = 0x1234_5678_9ABC_DEF0;
        let e = IdtEntry::interrupt_gate(handler, 0x08, 3);
        let lo = e.offset_lo;
        let mid = e.offset_mid;
        let hi = e.offset_hi;
        let sel = e.selector;
        let ist = e.ist;
        let ta = e.type_attr;
        let z = e.zero;
        assert_eq!(lo, 0xDEF0);
        assert_eq!(mid, 0x9ABC);
        assert_eq!(hi, 0x1234_5678);
        assert_eq!(sel, 0x08);
        assert_eq!(ist, 3);
        assert_eq!(ta, TYPE_ATTR_KERNEL_INTERRUPT);
        assert_eq!(ta, 0x8E);
        assert_eq!(z, 0);
        assert_eq!(e.handler(), handler);
    }

    #[test]
    fn interrupt_gate_masks_ist_to_3_bits() {
        // `ist = 0xFF` should be masked to `0x7`, not allowed to
        // smear into `type_attr`.
        let e = IdtEntry::interrupt_gate(0, 0x08, 0xFF);
        assert_eq!(e.ist, 0x7);
        assert_eq!(e.type_attr, TYPE_ATTR_KERNEL_INTERRUPT);
    }

    #[test]
    fn idt_pointer_layout() {
        assert_eq!(size_of::<IdtPointer>(), 10);
        assert_eq!(offset_of!(IdtPointer, limit), 0);
        assert_eq!(offset_of!(IdtPointer, base), 2);
    }

    #[test]
    fn with_default_handler_populates_all_slots() {
        // Track which exception vectors the `ist_for_exception`
        // closure was asked about; the closure should be invoked
        // exactly once per vector in 0..32.
        let mut asked = [false; 32];
        let idt = Idt::with_default_handler(0xAABB_CCDD_EE11_2233, 0x08, |v| {
            asked[v as usize] = true;
            // Use a non-zero IST only for #DF (vector 8) so the test
            // also covers the case where some-but-not-all exception
            // slots get an IST.
            u8::from(v == 8)
        });
        assert!(asked.iter().all(|x| *x));
        for (v, e) in idt.entries.iter().enumerate() {
            // Copy each packed field through a primitive local so the
            // `assert_eq!` macro doesn't take a misaligned reference
            // (E0793). The `*e` copy alone isn't enough — the field
            // alignment is a property of the struct definition.
            let handler = e.handler();
            let entry = *e;
            let selector = entry.selector;
            let type_attr = entry.type_attr;
            let ist = entry.ist;
            assert_eq!(handler, 0xAABB_CCDD_EE11_2233);
            assert_eq!(selector, 0x08);
            assert_eq!(type_attr, TYPE_ATTR_KERNEL_INTERRUPT);
            if v == 8 {
                assert_eq!(ist, 1);
            } else {
                assert_eq!(ist, 0);
            }
        }
    }

    #[test]
    fn saved_regs_layout_is_pinned() {
        assert_eq!(size_of::<SavedRegs>(), 15 * 8);
        // r15 is at offset 0 (lowest address) because the prologue
        // pushes it *last* and the stack grows downward.
        assert_eq!(offset_of!(SavedRegs, r15), 0);
        assert_eq!(offset_of!(SavedRegs, rax), 14 * 8);
    }

    #[test]
    fn interrupt_stack_frame_layout_is_pinned() {
        assert_eq!(size_of::<InterruptStackFrame>(), 5 * 8);
        assert_eq!(offset_of!(InterruptStackFrame, rip), 0);
        assert_eq!(offset_of!(InterruptStackFrame, cs), 8);
        assert_eq!(offset_of!(InterruptStackFrame, rflags), 16);
        assert_eq!(offset_of!(InterruptStackFrame, rsp), 24);
        assert_eq!(offset_of!(InterruptStackFrame, ss), 32);
    }
}
