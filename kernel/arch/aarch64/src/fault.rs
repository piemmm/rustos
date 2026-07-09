//! aarch64 synchronous-exception (fault) hook.
//!
//! The EL1 exception vector ([`crate::exceptions`]) routes an IRQ to the
//! timer/IPI path and an EL0 `svc` to the syscall path. Every *other*
//! synchronous exception — a data or instruction abort (page fault),
//! an alignment fault, an illegal-state exception — is, by default,
//! unrecoverable in this kernel slice: resuming the faulting instruction
//! without fix-up logic would re-trap forever, so the vector parks the
//! CPU (never silently reset).
//!
//! A single fault handler may be installed through [`set_fault_handler`]
//! before any fault can fire; the vector then invokes it with the
//! decoded `ESR_EL1` (exception syndrome), `FAR_EL1` (faulting address),
//! and `ELR_EL1` (faulting PC). It is the aarch64 analogue of the riscv64
//! `fault` hook and the x86_64 page-fault callback: the memory-isolation
//! QEMU vertical installs one that confirms the attacker faulted on the
//! isolated address and reports the result to QEMU. The handler must not
//! return — see [`FaultHandlerFn`].
//!
//! # No global mutable state
//!
//! The slot is set-once, backed by an atomic the vector reads without a
//! lock; a second publish fails closed. The `ESR_EL1`
//! decode and the slot build on the host, so their unit tests run under
//! `cargo test`; only the system-register reads that feed the handler
//! are gated to the freestanding aarch64 target (in
//! [`crate::exceptions`]).

use core::sync::atomic::{AtomicUsize, Ordering};

/// Shift of the `ESR_ELx.EC` (exception class) field (bits `[31:26]`,
/// ARM ARM D17.2.37).
pub const ESR_EC_SHIFT: u64 = 26;

/// Mask of the `ESR_ELx.EC` field after shifting.
pub const ESR_EC_MASK: u64 = 0x3F;

/// `EC` for a Data Abort taken from a lower EL (e.g. EL0 reading an
/// unmapped user address). ARM ARM Table D17-2.
pub const EC_DATA_ABORT_LOWER: u64 = 0b10_0100;

/// `EC` for a Data Abort taken from the current EL (e.g. EL1 reading an
/// address the active translation regime does not map). The
/// memory-isolation vertical expects exactly this when the attacker
/// space reads the victim-only address.
pub const EC_DATA_ABORT_SAME: u64 = 0b10_0101;

/// `EC` for an Instruction Abort taken from a lower EL.
pub const EC_INSTRUCTION_ABORT_LOWER: u64 = 0b10_0000;

/// `EC` for an Instruction Abort taken from the current EL.
pub const EC_INSTRUCTION_ABORT_SAME: u64 = 0b10_0001;

/// Extract the exception class (`EC`) from a raw `ESR_EL1` value.
#[must_use]
pub const fn exception_class(esr: u64) -> u64 {
    (esr >> ESR_EC_SHIFT) & ESR_EC_MASK
}

/// `true` iff `esr` denotes a data abort taken from a lower EL — an EL0
/// user access that could not be translated. This is the only exception
/// class the demand-paged file-mapping resolver may attempt to resolve:
/// a kernel-mode abort or an instruction abort is never file backing and
/// always takes the fatal path.
#[must_use]
pub const fn is_lower_el_data_abort(esr: u64) -> bool {
    exception_class(esr) == EC_DATA_ABORT_LOWER
}

/// `true` iff `esr` denotes a data or instruction abort (a page fault),
/// taken from either the current or a lower EL.
#[must_use]
pub const fn is_abort(esr: u64) -> bool {
    matches!(
        exception_class(esr),
        EC_DATA_ABORT_LOWER
            | EC_DATA_ABORT_SAME
            | EC_INSTRUCTION_ABORT_LOWER
            | EC_INSTRUCTION_ABORT_SAME
    )
}

/// Signature of the fault handler the vector invokes for an unexpected
/// synchronous exception.
///
/// `esr` is the raw `ESR_EL1` syndrome, `far` the `FAR_EL1` faulting
/// address (for an abort, the address that could not be translated), and
/// `elr` the `ELR_EL1` PC of the faulting instruction. The handler
/// **must not return**: this kernel slice has no fix-up logic to resume
/// the faulting instruction, so a return would re-trap forever. Test
/// handlers report the outcome to QEMU through [`crate::qemu_exit`].
pub type FaultHandlerFn = extern "C" fn(esr: u64, far: u64, elr: u64) -> !;

/// Slot holding the installed fault handler as a raw function pointer
/// (`0` = none installed).
static FAULT_HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Failure modes of [`set_fault_handler`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SetFaultHandlerError {
    /// A handler was already published; the slot is set-once per boot.
    AlreadyInstalled,
}

/// Install the synchronous-exception fault handler.
///
/// Must be called once, on the boot CPU, before any fault can fire.
///
/// # Errors
///
/// [`SetFaultHandlerError::AlreadyInstalled`] on the second publish.
pub fn set_fault_handler(cb: FaultHandlerFn) -> Result<(), SetFaultHandlerError> {
    let raw = cb as usize;
    FAULT_HANDLER
        .compare_exchange(0, raw, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| SetFaultHandlerError::AlreadyInstalled)
}

/// Read back the installed fault handler, if any. The vector calls this
/// on an unexpected synchronous exception; it is also a test/diagnostic
/// observer.
#[must_use]
pub fn fault_handler() -> Option<FaultHandlerFn> {
    let raw = FAULT_HANDLER.load(Ordering::Acquire);
    if raw == 0 {
        None
    } else {
        // SAFETY: every value stored into the slot round-trips a valid
        // `FaultHandlerFn` through `set_fault_handler`; function pointers
        // are `usize`-sized so the transmute is lossless.
        Some(unsafe { core::mem::transmute::<usize, FaultHandlerFn>(raw) })
    }
}

#[cfg(test)]
fn clear_fault_handler_for_tests() {
    // Test-only: lets back-to-back host tests reinstall a handler.
    // Production code never clears the slot.
    FAULT_HANDLER.store(0, Ordering::Release);
}

/// Signature of the user-fault resolver the vector offers a lower-EL data
/// abort to before the fatal path.
///
/// `far` is the `FAR_EL1` faulting address. A `true` return means the
/// fault is dealt with and the vector simply returns — `ELR_EL1` still
/// points at the faulting instruction, so the `eret` retries the access
/// against the now-resident page. A `false` return means the fault was
/// not (and will never be) resolvable and the vector falls through to the
/// fatal [`FaultHandlerFn`] path. The callback may also *not return* for
/// the faulting task: when the fault is fatal to the task alone, the
/// binary's callback suspends it into the scheduler with an exit action
/// and the vector call never completes on that stack — exactly like a
/// rescheduling syscall. Like every trap-path callback it is a bare
/// `extern "C" fn` with no captured environment.
pub type UserFaultResolveFn = extern "C" fn(far: u64) -> bool;

/// Slot holding the installed user-fault resolver as a raw function
/// pointer (`0` = none installed).
static USER_FAULT_RESOLVER: AtomicUsize = AtomicUsize::new(0);

/// Install the user-fault resolver.
///
/// Must be called once, on the boot CPU, before user space is entered
/// (the syscall-dispatch ordering contract). Without one installed every
/// lower-EL data abort takes the fatal path — fail closed, exactly as
/// before demand paging existed.
///
/// # Errors
///
/// [`SetFaultHandlerError::AlreadyInstalled`] on the second publish.
pub fn set_user_fault_resolver(cb: UserFaultResolveFn) -> Result<(), SetFaultHandlerError> {
    let raw = cb as usize;
    USER_FAULT_RESOLVER
        .compare_exchange(0, raw, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| SetFaultHandlerError::AlreadyInstalled)
}

/// Read back the installed user-fault resolver, if any. The vector calls
/// this on a lower-EL data abort; it is also a test/diagnostic observer.
#[must_use]
pub fn user_fault_resolver() -> Option<UserFaultResolveFn> {
    let raw = USER_FAULT_RESOLVER.load(Ordering::Acquire);
    if raw == 0 {
        None
    } else {
        // SAFETY: every value stored into the slot round-trips a valid
        // `UserFaultResolveFn` through `set_user_fault_resolver`; function
        // pointers are `usize`-sized so the transmute is lossless.
        Some(unsafe { core::mem::transmute::<usize, UserFaultResolveFn>(raw) })
    }
}

#[cfg(test)]
fn clear_user_fault_resolver_for_tests() {
    // Test-only: lets back-to-back host tests reinstall a resolver.
    // Production code never clears the slot.
    USER_FAULT_RESOLVER.store(0, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aborts_are_recognised() {
        assert!(is_abort(EC_DATA_ABORT_SAME << ESR_EC_SHIFT));
        assert!(is_abort(EC_DATA_ABORT_LOWER << ESR_EC_SHIFT));
        assert!(is_abort(EC_INSTRUCTION_ABORT_SAME << ESR_EC_SHIFT));
        assert!(is_abort(EC_INSTRUCTION_ABORT_LOWER << ESR_EC_SHIFT));
    }

    #[test]
    fn non_aborts_are_not_recognised() {
        // EC 0b010101 is an SVC from AArch64 (a syscall), not an abort.
        assert!(!is_abort(0b01_0101 << ESR_EC_SHIFT));
        // EC 0 is "unknown reason".
        assert!(!is_abort(0));
    }

    #[test]
    fn exception_class_extracts_the_ec_field() {
        let esr = (EC_DATA_ABORT_SAME << ESR_EC_SHIFT) | 0x37; // ISS noise
        assert_eq!(exception_class(esr), EC_DATA_ABORT_SAME);
    }

    #[test]
    fn ec_codes_match_arm_arm() {
        assert_eq!(EC_DATA_ABORT_LOWER, 0x24);
        assert_eq!(EC_DATA_ABORT_SAME, 0x25);
        assert_eq!(EC_INSTRUCTION_ABORT_LOWER, 0x20);
        assert_eq!(EC_INSTRUCTION_ABORT_SAME, 0x21);
    }

    #[test]
    fn lower_el_data_aborts_are_distinguished() {
        assert!(is_lower_el_data_abort(EC_DATA_ABORT_LOWER << ESR_EC_SHIFT));
        // Same-EL data aborts, instruction aborts, and an EL0 `svc` are
        // never offered to the user-fault resolver.
        assert!(!is_lower_el_data_abort(EC_DATA_ABORT_SAME << ESR_EC_SHIFT));
        assert!(!is_lower_el_data_abort(
            EC_INSTRUCTION_ABORT_LOWER << ESR_EC_SHIFT
        ));
        assert!(!is_lower_el_data_abort(0b01_0101 << ESR_EC_SHIFT));
    }

    extern "C" fn host_fault_handler(_esr: u64, _far: u64, _elr: u64) -> ! {
        panic!("host test handler must never be invoked");
    }

    extern "C" fn host_user_fault_resolver(_far: u64) -> bool {
        false
    }

    #[test]
    fn user_fault_resolver_slot_is_set_once_and_round_trips() {
        clear_user_fault_resolver_for_tests();
        assert!(user_fault_resolver().is_none());

        set_user_fault_resolver(host_user_fault_resolver).expect("first install");
        let got = user_fault_resolver().expect("resolver present");
        assert_eq!(
            got as *const () as usize,
            host_user_fault_resolver as *const () as usize
        );

        assert_eq!(
            set_user_fault_resolver(host_user_fault_resolver),
            Err(SetFaultHandlerError::AlreadyInstalled)
        );
        clear_user_fault_resolver_for_tests();
    }

    #[test]
    fn slot_is_set_once_and_round_trips() {
        clear_fault_handler_for_tests();
        assert!(fault_handler().is_none());

        set_fault_handler(host_fault_handler).expect("first install");
        let got = fault_handler().expect("handler present");
        assert_eq!(
            got as *const () as usize,
            host_fault_handler as *const () as usize
        );

        assert_eq!(
            set_fault_handler(host_fault_handler),
            Err(SetFaultHandlerError::AlreadyInstalled)
        );
        clear_fault_handler_for_tests();
    }
}
