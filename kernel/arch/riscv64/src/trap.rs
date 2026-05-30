//! riscv64 S-mode trap vector and external-interrupt dispatch seam.
//!
//! Stage 4.D Item 4 (riscv64 external-IRQ trap glue). This module owns
//! the architecture-specific surface for taking a supervisor-mode trap
//! on the QEMU `virt` board:
//!
//! * The `stvec` trap vector (`rustos_riscv64_trap_vector`, published
//!   by the `global_asm!` block below) that saves the interrupted
//!   context's caller-saved registers, calls the Rust handler, restores,
//!   and `sret`s.
//! * `init_traps`, which points `stvec` at that vector (direct mode)
//!   and enables supervisor external interrupts (`sie.SEIE`) plus the
//!   supervisor global interrupt-enable (`sstatus.SIE`).
//! * The one-shot-published dispatch callback ([`set_trap_dispatch`])
//!   the Rust handler invokes on a supervisor *external* interrupt. The
//!   callback is responsible for claiming the pending source from the
//!   [`crate::plic::PlicController`], forwarding it to
//!   `rustos_kernel_irq::IrqTable::fire` (which masks the source before
//!   the waiter observes `ready`), and completing the claim.
//!
//! # Mask-before-wake
//!
//! Like the x86_64 dispatcher, the Rust handler here does **not** mask
//! the line itself — it forwards to the installed dispatcher, which
//! calls `IrqTable::fire`, which in turn calls the
//! [`crate::plic::PlicController`]'s `mask` *before* setting `ready`
//! (`docs/src/security/irq.md`). The PLIC claim/complete handshake
//! belongs to the dispatcher because only it holds the controller
//! reference.
//!
//! # No global mutable state
//!
//! The dispatch slot is set-once at boot, backed by an atomic so the
//! trap path reads it without a lock; a second publish fails closed
//! (`AGENTS.md` §2.1).
//!
//! (`rustos_riscv64_trap_vector`, `init_traps`, and the Rust handler are
//! gated to the freestanding riscv64 target, so they are plain text on
//! host doc builds; the dispatch slot and `scause` decode build on the
//! host so their unit tests run under `cargo test`.)

use core::sync::atomic::{AtomicUsize, Ordering};

/// `scause` bit set when the trap is an interrupt (cleared for a
/// synchronous exception). The remaining bits hold the cause code.
pub const SCAUSE_INTERRUPT_BIT: u64 = 1 << 63;

/// `scause` cause code for a Supervisor External Interrupt (the cause
/// the PLIC raises). Privileged-spec table 4.2.
pub const SCAUSE_SUPERVISOR_EXTERNAL: u64 = 9;

/// `sie.SEIE` — supervisor external interrupt enable (bit 9).
pub const SIE_SEIE: u64 = 1 << 9;

/// `sstatus.SIE` — supervisor global interrupt enable (bit 1).
pub const SSTATUS_SIE: u64 = 1 << 1;

/// `true` iff `scause` denotes a supervisor external interrupt — the
/// interrupt bit is set *and* the cause code is
/// [`SCAUSE_SUPERVISOR_EXTERNAL`].
#[must_use]
pub const fn is_supervisor_external_interrupt(scause: u64) -> bool {
    (scause & SCAUSE_INTERRUPT_BIT) != 0
        && (scause & !SCAUSE_INTERRUPT_BIT) == SCAUSE_SUPERVISOR_EXTERNAL
}

/// Signature of the installed external-interrupt dispatcher.
///
/// Invoked from the trap handler with interrupts disabled (the trap
/// entered with `sstatus.SIE` cleared by hardware). The dispatcher must
/// claim the pending PLIC source, forward it to
/// `rustos_kernel_irq::IrqTable::fire`, and complete the claim. It must
/// not allocate, block, or re-enter the scheduler.
pub type TrapDispatchFn = extern "C" fn();

/// Slot holding the installed dispatcher as a raw function pointer.
static TRAP_DISPATCH_FN: AtomicUsize = AtomicUsize::new(0);

/// Failure modes of [`set_trap_dispatch`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SetDispatchError {
    /// A dispatcher was already published; the slot is set-once per
    /// boot (`AGENTS.md` §2.1).
    AlreadyInstalled,
}

/// Install the external-interrupt dispatcher.
///
/// # Errors
///
/// [`SetDispatchError::AlreadyInstalled`] on the second publish.
pub fn set_trap_dispatch(cb: TrapDispatchFn) -> Result<(), SetDispatchError> {
    let raw = cb as usize;
    TRAP_DISPATCH_FN
        .compare_exchange(0, raw, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| SetDispatchError::AlreadyInstalled)
}

/// Address of the installed dispatcher (`0` if none). Test/diagnostic
/// observer.
#[must_use]
pub fn trap_dispatch_addr() -> usize {
    TRAP_DISPATCH_FN.load(Ordering::Acquire)
}

#[cfg(test)]
fn clear_trap_dispatch_for_tests() {
    // Test-only: lets back-to-back host tests reinstall a dispatcher.
    // Production code never clears the slot (`AGENTS.md` §2.1).
    TRAP_DISPATCH_FN.store(0, Ordering::Release);
}

// --- Freestanding trap vector + Rust handler ----------------------

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
core::arch::global_asm!(include_str!("trap.s"));

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
extern "C" {
    /// S-mode trap vector published by `trap.s`. Installed into `stvec`
    /// by [`init_traps`]; never called from Rust.
    fn rustos_riscv64_trap_vector();
}

/// Point `stvec` at the trap vector (direct mode) and enable supervisor
/// external interrupts.
///
/// # Safety
///
/// Must be called once, on the boot hart, after a stack is established
/// and before any interrupt source is armed. Enabling `sstatus.SIE`
/// makes the hart take interrupts; the caller must have installed the
/// dispatcher ([`set_trap_dispatch`]) and a valid `stvec`-aligned vector
/// first (this function installs the latter).
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub unsafe fn init_traps() {
    let base = rustos_riscv64_trap_vector as *const () as usize;
    // SAFETY: `base` is the 4-byte-aligned address of the asm trap
    // vector (direct mode encodes mode 0 in the low two bits, which are
    // zero by the `.align 2`). Writing `stvec`, setting `sie.SEIE`, and
    // setting `sstatus.SIE` are the documented S-mode interrupt-enable
    // sequence; none has memory side effects beyond the CSRs named.
    unsafe {
        core::arch::asm!("csrw stvec, {}", in(reg) base, options(nomem, nostack));
        core::arch::asm!("csrs sie, {}", in(reg) SIE_SEIE, options(nomem, nostack));
        core::arch::asm!("csrs sstatus, {}", in(reg) SSTATUS_SIE, options(nomem, nostack));
    }
}

/// Rust entry invoked by the asm trap vector.
///
/// Reads `scause`; on a supervisor external interrupt it forwards to the
/// installed dispatcher (which performs the PLIC claim → `IrqTable::fire`
/// → complete handshake). Any synchronous exception is unexpected in the
/// boot-to-`BootCompleted` slice and fails closed by parking the hart
/// rather than `sret`-looping on the faulting instruction (`AGENTS.md`
/// §2 — never silently reset; §5.4.5 — fail closed).
///
/// # Safety
///
/// Only callable from `rustos_riscv64_trap_vector`, which has saved the
/// interrupted context's caller-saved registers.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
#[no_mangle]
unsafe extern "C" fn rustos_riscv64_trap_handler() {
    let scause: u64;
    // SAFETY: reading `scause` has no side effects.
    unsafe {
        core::arch::asm!("csrr {}, scause", out(reg) scause, options(nomem, nostack));
    }

    if (scause & SCAUSE_INTERRUPT_BIT) == 0 {
        // Synchronous exception: nothing in this slice should fault, and
        // returning would re-execute the faulting instruction forever.
        crate::kernel_arch::halt_current_hart();
    }

    if is_supervisor_external_interrupt(scause) {
        let raw = TRAP_DISPATCH_FN.load(Ordering::Acquire);
        if raw != 0 {
            // SAFETY: every value stored into the slot round-trips a
            // valid `TrapDispatchFn` through `set_trap_dispatch`;
            // function pointers are `usize`-sized so the transmute is
            // lossless.
            let cb: TrapDispatchFn = unsafe { core::mem::transmute::<usize, TrapDispatchFn>(raw) };
            cb();
        }
        // With no dispatcher installed the source stays pending and
        // masked at the controller until one is; the timer interrupt or
        // a later claim drains it. A pre-dispatch external interrupt is
        // impossible in practice (the boot path installs the dispatcher
        // before arming any line).
    }

    // Other interrupt causes (software, timer) are not enabled in this
    // slice; falling through resumes the interrupted context via `sret`.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervisor_external_interrupt_is_recognised() {
        // Interrupt bit set, cause 9.
        assert!(is_supervisor_external_interrupt(
            SCAUSE_INTERRUPT_BIT | SCAUSE_SUPERVISOR_EXTERNAL
        ));
    }

    #[test]
    fn synchronous_exception_with_code_9_is_not_an_external_interrupt() {
        // Same code, but interrupt bit clear → it is exception 9
        // (store/AMO access fault), not the PLIC external interrupt.
        assert!(!is_supervisor_external_interrupt(
            SCAUSE_SUPERVISOR_EXTERNAL
        ));
    }

    #[test]
    fn other_interrupt_causes_are_rejected() {
        // Supervisor timer interrupt is cause 5 with the interrupt bit.
        assert!(!is_supervisor_external_interrupt(SCAUSE_INTERRUPT_BIT | 5));
        // Supervisor software interrupt is cause 1.
        assert!(!is_supervisor_external_interrupt(SCAUSE_INTERRUPT_BIT | 1));
    }

    #[test]
    fn enable_bit_constants_match_privileged_spec() {
        assert_eq!(SIE_SEIE, 0x200);
        assert_eq!(SSTATUS_SIE, 0x2);
        assert_eq!(SCAUSE_INTERRUPT_BIT, 0x8000_0000_0000_0000);
    }

    extern "C" fn host_dispatch_cb() {}

    #[test]
    fn set_trap_dispatch_fails_closed_on_second_install() {
        clear_trap_dispatch_for_tests();
        set_trap_dispatch(host_dispatch_cb).expect("first install");
        assert_eq!(
            set_trap_dispatch(host_dispatch_cb),
            Err(SetDispatchError::AlreadyInstalled)
        );
        clear_trap_dispatch_for_tests();
    }

    #[test]
    fn trap_dispatch_addr_round_trips_installed_fn() {
        clear_trap_dispatch_for_tests();
        set_trap_dispatch(host_dispatch_cb).expect("install");
        assert_eq!(trap_dispatch_addr(), host_dispatch_cb as *const () as usize);
        clear_trap_dispatch_for_tests();
    }
}
