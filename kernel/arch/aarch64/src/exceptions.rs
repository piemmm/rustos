//! EL1 exception handling: vector-table install, IRQ dispatch, and the
//! synchronous-fault path.
//!
//! The assembly vector table (`vectors.s`, included via `global_asm!` in
//! `lib.rs`) tags every exception with a numeric *kind* and calls
//! `rustos_aarch64_trap_handler`. This module:
//!
//! * `init_vectors` points `VBAR_EL1` at the table.
//! * `enable_irq` unmasks IRQs at the PE (`DAIF.I`), the aarch64
//!   analogue of riscv64's `sstatus.SIE` enable — kept separate from
//!   arming the timer so the caller controls exactly when ticks begin.
//! * `rustos_aarch64_trap_handler` dispatches an IRQ to the GIC
//!   acknowledge → timer/SGI → end-of-interrupt handshake, and routes an
//!   unexpected synchronous exception to the installed [`crate::fault`]
//!   handler (or fails closed by parking the CPU).
//!
//! # What is not here yet
//!
//! The live EL0 `svc` syscall dispatch (synchronous exception from a
//! lower EL) is a follow-up: the argument-marshalling logic lives in
//! [`crate::syscall_entry`] and is host-tested, but no vertical enters
//! EL0 yet, so a synchronous exception that is not an abort fails closed
//! here (`AGENTS.md` §5.4.5) rather than returning to an unspecified
//! state. Wiring the saved register frame through to `dispatch_svc` is
//! the aarch64 analogue of riscv64's remaining syscall work.
//!
//! The handler and the CSR writes are freestanding-only; the exception
//! *kind* constants and their classification build on the host so their
//! unit tests run under `cargo test`.

/// Exception kinds the vector table tags each entry with, matching the
/// `mov x0, #N` immediates in `vectors.s` (entry index `0..16`).
pub mod kind {
    /// Current EL with SP0 — IRQ. Not used (the kernel runs on `SP_EL1`)
    /// but classified so a stray entry is still dispatched correctly.
    pub const CUR_SP0_IRQ: u64 = 1;
    /// Current EL with SPx — Synchronous (a kernel-mode fault).
    pub const CUR_SPX_SYNC: u64 = 4;
    /// Current EL with SPx — IRQ (the timer / SGI path).
    pub const CUR_SPX_IRQ: u64 = 5;
    /// Lower EL (AArch64) — Synchronous (an EL0 `svc` or user fault).
    pub const LOWER_SYNC: u64 = 8;
    /// Lower EL (AArch64) — IRQ.
    pub const LOWER_IRQ: u64 = 9;
}

/// `true` iff `kind` denotes an IRQ entry (from any EL the kernel may be
/// interrupted in).
#[must_use]
pub const fn is_irq(kind: u64) -> bool {
    matches!(
        kind,
        kind::CUR_SP0_IRQ | kind::CUR_SPX_IRQ | kind::LOWER_IRQ
    )
}

/// `true` iff `kind` denotes a synchronous-exception entry the fault path
/// handles (current-EL or lower-EL synchronous).
#[must_use]
pub const fn is_sync(kind: u64) -> bool {
    matches!(kind, kind::CUR_SPX_SYNC | kind::LOWER_SYNC)
}

// --- Freestanding vector install + dispatch -----------------------

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
extern "C" {
    /// EL1 vector table published by `vectors.s`. Installed into
    /// `VBAR_EL1` by [`init_vectors`]; never called from Rust.
    fn rustos_aarch64_vectors();
}

/// Point `VBAR_EL1` at the exception vector table.
///
/// # Safety
///
/// Must be called once, on the boot CPU, after a stack is established
/// and before interrupts are unmasked. The table is 2 KiB aligned by
/// `vectors.s`, satisfying the `VBAR_EL1` alignment requirement.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn init_vectors() {
    let base = rustos_aarch64_vectors as *const () as u64;
    // SAFETY: `base` is the 2 KiB-aligned address of the asm vector
    // table; writing it to `VBAR_EL1` has no side effect beyond the
    // system register.
    unsafe {
        core::arch::asm!("msr VBAR_EL1, {}", in(reg) base, options(nomem, nostack));
    }
}

/// Unmask IRQs at the PE (`DAIF.I`), allowing the CPU to take interrupts.
///
/// Like riscv64's `sstatus.SIE` enable, this is deliberately separate
/// from arming a source ([`crate::preempt::init_local_preempt`]): the
/// caller unmasks only once the vector table and the source are in
/// place.
///
/// # Safety
///
/// The caller must have installed the vector table ([`init_vectors`])
/// and any IRQ handler state (the timer callback) first; otherwise an
/// in-flight interrupt would dispatch through an unset slot.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn enable_irq() {
    // SAFETY: clearing `DAIF.I` (the IRQ mask, immediate bit 1) unmasks
    // IRQs; it has no other side effect.
    unsafe {
        core::arch::asm!("msr DAIFClr, #2", options(nomem, nostack));
    }
}

/// Read the `ESR_EL1` exception syndrome.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn read_esr() -> u64 {
    let esr: u64;
    // SAFETY: reading `ESR_EL1` has no side effects.
    unsafe {
        core::arch::asm!("mrs {}, ESR_EL1", out(reg) esr, options(nomem, nostack, preserves_flags));
    }
    esr
}

/// Read the `FAR_EL1` faulting virtual address.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn read_far() -> u64 {
    let far: u64;
    // SAFETY: reading `FAR_EL1` has no side effects.
    unsafe {
        core::arch::asm!("mrs {}, FAR_EL1", out(reg) far, options(nomem, nostack, preserves_flags));
    }
    far
}

/// Read the `ELR_EL1` faulting / return PC.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn read_elr() -> u64 {
    let elr: u64;
    // SAFETY: reading `ELR_EL1` has no side effects.
    unsafe {
        core::arch::asm!("mrs {}, ELR_EL1", out(reg) elr, options(nomem, nostack, preserves_flags));
    }
    elr
}

/// Handle an IRQ: acknowledge the GIC, dispatch the timer PPI to the
/// scheduler-tick path, then complete the interrupt.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn handle_irq() {
    let intid = crate::gic::acknowledge();
    if intid == crate::gic::SPURIOUS_INTID {
        // Spurious read: nothing pending, and the GIC requires no EOI.
        return;
    }
    if intid == crate::preempt::TIMER_PPI {
        // Single-CPU slice: the boot CPU is logical CPU 0.
        crate::preempt::on_timer_interrupt(0);
    }
    // Complete every acknowledged interrupt (timer, SGI/IPI, or other)
    // so the CPU interface does not wedge with an active priority.
    crate::gic::end_of_interrupt(intid);
}

/// Rust entry invoked by the asm vector trampoline with the exception
/// `kind`.
///
/// # Safety
///
/// Only callable from `rustos_aarch64_trap_common`, which has saved the
/// interrupted GP registers and tagged the exception kind. An IRQ
/// returns (the trampoline `eret`s); a fault diverges (the installed
/// handler or the park never returns).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[no_mangle]
unsafe extern "C" fn rustos_aarch64_trap_handler(kind: u64) {
    if is_irq(kind) {
        handle_irq();
        return;
    }

    if is_sync(kind) {
        // An unexpected synchronous exception (an abort, or an EL0 `svc`
        // before the syscall frame wiring lands). Forward to the
        // installed fault handler if present (the memory-isolation
        // vertical installs one); otherwise fail closed by parking.
        if let Some(handler) = crate::fault::fault_handler() {
            handler(read_esr(), read_far(), read_elr());
        }
        crate::kernel_arch::halt_current_cpu();
    }

    // FIQ / SError / AArch32 entries are not expected in this slice.
    // Park rather than `eret`-looping on an unhandled condition
    // (`AGENTS.md` §2 — never silently reset).
    crate::kernel_arch::halt_current_cpu();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn irq_kinds_are_classified() {
        assert!(is_irq(kind::CUR_SP0_IRQ));
        assert!(is_irq(kind::CUR_SPX_IRQ));
        assert!(is_irq(kind::LOWER_IRQ));
        assert!(!is_irq(kind::CUR_SPX_SYNC));
    }

    #[test]
    fn sync_kinds_are_classified() {
        assert!(is_sync(kind::CUR_SPX_SYNC));
        assert!(is_sync(kind::LOWER_SYNC));
        assert!(!is_sync(kind::CUR_SPX_IRQ));
    }

    #[test]
    fn kind_values_match_vector_table_indices() {
        // The `mov x0, #N` immediates in `vectors.s` use these indices.
        assert_eq!(kind::CUR_SP0_IRQ, 1);
        assert_eq!(kind::CUR_SPX_SYNC, 4);
        assert_eq!(kind::CUR_SPX_IRQ, 5);
        assert_eq!(kind::LOWER_SYNC, 8);
        assert_eq!(kind::LOWER_IRQ, 9);
    }
}
