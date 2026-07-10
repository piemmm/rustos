//! riscv64 synchronous-exception (fault) hook.
//!
//! The S-mode trap vector ([`crate::trap`]) routes a U-mode `ecall` to
//! the syscall path and a supervisor timer / external interrupt to their
//! dispatchers. Every *other* synchronous exception — an instruction,
//! load, or store/AMO page fault, an access fault, an illegal
//! instruction — is, by default, unrecoverable in this kernel slice:
//! resuming the faulting instruction without fix-up logic would re-trap
//! forever, so the trap path parks the hart (never
//! silently reset).
//!
//! A single fault handler may be installed through [`set_fault_handler`]
//! before any fault can fire; the trap path then invokes it, passing the
//! decoded `scause`, the faulting address (`stval`), and the faulting PC
//! (`sepc`). It is the riscv64 analogue of the x86_64 page-fault callback
//! (`kernel/arch/x86_64::idt`): the memory-isolation QEMU vertical
//! installs one that confirms the attacker faulted on the isolated
//! address and reports the result to QEMU. The handler must not return —
//! see [`FaultHandlerFn`].
//!
//! # No global mutable state
//!
//! The slot is set-once, backed by an atomic the trap path reads without
//! a lock; a second publish fails closed. The `scause`
//! decode and the slot build on the host, so their unit tests run under
//! `cargo test`; only the CSR reads that feed the handler are gated to
//! the freestanding riscv64 target (in [`crate::trap`]).

use core::sync::atomic::{AtomicUsize, Ordering};

/// `scause` cause code for an Instruction page fault (privileged spec
/// table 4.2).
pub const SCAUSE_INSTRUCTION_PAGE_FAULT: u64 = 12;

/// `scause` cause code for a Load page fault — the cause raised when a
/// hart reads a virtual address that is unmapped (or lacks read
/// permission) in the active page table. The memory-isolation vertical
/// expects exactly this when the attacker reads the victim-only address.
pub const SCAUSE_LOAD_PAGE_FAULT: u64 = 13;

/// `scause` cause code for a Store/AMO page fault (privileged spec
/// table 4.2).
pub const SCAUSE_STORE_PAGE_FAULT: u64 = 15;

/// `true` iff `scause` denotes one of the three page-fault causes (the
/// interrupt bit is clear and the cause code is an instruction, load, or
/// store/AMO page fault).
#[must_use]
pub const fn is_page_fault(scause: u64) -> bool {
    if (scause & crate::trap::SCAUSE_INTERRUPT_BIT) != 0 {
        return false;
    }
    matches!(
        scause,
        SCAUSE_INSTRUCTION_PAGE_FAULT | SCAUSE_LOAD_PAGE_FAULT | SCAUSE_STORE_PAGE_FAULT
    )
}

/// `true` iff `scause` denotes a **load** page fault — the only class
/// the demand-paged file-mapping resolver may attempt to *resolve*. An
/// instruction page fault is never file backing (a file mapping is never
/// executable) and is not offered at all.
#[must_use]
pub const fn is_load_page_fault(scause: u64) -> bool {
    if (scause & crate::trap::SCAUSE_INTERRUPT_BIT) != 0 {
        return false;
    }
    scause == SCAUSE_LOAD_PAGE_FAULT
}

/// `true` iff `scause` denotes a **store/AMO** page fault. It is offered
/// to the resolver with `write = true`, which never resolves it: a file
/// mapping is read-only, so a store to it can never be made valid — and
/// once the target page is resident, resolving a store fault as "already
/// resident, retry" would re-execute the store into an endless fault
/// storm. The resolver kills the faulting task instead, so a store to a
/// read-only mapping (or any wild write) costs the task, never the hart.
#[must_use]
pub const fn is_store_page_fault(scause: u64) -> bool {
    if (scause & crate::trap::SCAUSE_INTERRUPT_BIT) != 0 {
        return false;
    }
    scause == SCAUSE_STORE_PAGE_FAULT
}

/// Signature of the fault handler the trap path invokes for an
/// unexpected synchronous exception.
///
/// `scause` is the raw trap cause, `stval` the faulting address (for a
/// page fault, the virtual address that could not be translated), and
/// `sepc` the PC of the faulting instruction. The handler **must not
/// return**: this kernel slice has no fix-up logic to resume the faulting
/// instruction, so a return would re-trap forever. Test handlers report
/// the outcome to QEMU through [`crate::qemu_exit`].
pub type FaultHandlerFn = extern "C" fn(scause: u64, stval: u64, sepc: u64) -> !;

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
/// Must be called once, on the boot hart, before any fault can fire.
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

/// Read back the installed fault handler, if any. The trap path calls
/// this on an unexpected synchronous exception; it is also a
/// test/diagnostic observer.
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

/// Signature of the user-fault resolver the trap path offers a U-mode
/// data page fault to before the fatal path.
///
/// `stval` is the faulting address and `write` the store/AMO `scause`
/// verdict (`true` = the access was a store). A `true` return means the
/// fault is dealt with and the trap path simply returns — the saved
/// `sepc` still points at the faulting instruction, so the `sret`
/// retries the access against the now-resident page; only a load is
/// ever resolved this way (file mappings are read-only). A `false`
/// return means the fault was not (and will never be) resolvable and
/// the trap path falls through to the fatal [`FaultHandlerFn`] path.
/// The callback may also *not return* for the faulting task: when the
/// fault is fatal to the task alone — every store, and any unresolvable
/// load — the binary's callback suspends it into the scheduler with an
/// exit action — exactly like a rescheduling syscall, so the task dies
/// and the hart never halts. Like every trap-path callback it is a bare
/// `extern "C" fn` with no captured environment.
pub type UserFaultResolveFn = extern "C" fn(stval: u64, write: bool) -> bool;

/// Slot holding the installed user-fault resolver as a raw function
/// pointer (`0` = none installed).
static USER_FAULT_RESOLVER: AtomicUsize = AtomicUsize::new(0);

/// Install the user-fault resolver.
///
/// Must be called once, on the boot hart, before user space is entered.
/// Without one installed every U-mode data page fault takes the fatal
/// path — fail closed, exactly as before demand paging existed.
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

/// Read back the installed user-fault resolver, if any. The trap path
/// calls this on a U-mode data page fault; it is also a test/diagnostic
/// observer.
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
fn clear_fault_handler_for_tests() {
    // Test-only: lets back-to-back host tests reinstall a handler.
    // Production code never clears the slot.
    FAULT_HANDLER.store(0, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trap::SCAUSE_INTERRUPT_BIT;

    #[test]
    fn page_fault_causes_are_recognised() {
        assert!(is_page_fault(SCAUSE_INSTRUCTION_PAGE_FAULT));
        assert!(is_page_fault(SCAUSE_LOAD_PAGE_FAULT));
        assert!(is_page_fault(SCAUSE_STORE_PAGE_FAULT));
    }

    #[test]
    fn interrupts_are_not_page_faults() {
        // The interrupt bit set with cause 13 is a (non-existent) async
        // cause, never the synchronous load page fault.
        assert!(!is_page_fault(
            SCAUSE_INTERRUPT_BIT | SCAUSE_LOAD_PAGE_FAULT
        ));
    }

    #[test]
    fn unrelated_exceptions_are_not_page_faults() {
        // Cause 2 is an illegal instruction; cause 8 is an ecall.
        assert!(!is_page_fault(2));
        assert!(!is_page_fault(8));
    }

    #[test]
    fn cause_codes_match_privileged_spec() {
        assert_eq!(SCAUSE_INSTRUCTION_PAGE_FAULT, 12);
        assert_eq!(SCAUSE_LOAD_PAGE_FAULT, 13);
        assert_eq!(SCAUSE_STORE_PAGE_FAULT, 15);
    }

    #[test]
    fn load_and_store_page_faults_are_classified_for_the_resolver() {
        // A load page fault is the resolvable class (demand-paged file
        // backing); a store/AMO page fault is offered with `write = true`
        // and is always fatal to the task (file mappings are read-only —
        // resolving a store against a resident page would retry it
        // forever, and before this classification a user store could park
        // the whole hart).
        assert!(is_load_page_fault(SCAUSE_LOAD_PAGE_FAULT));
        assert!(!is_load_page_fault(SCAUSE_STORE_PAGE_FAULT));
        assert!(is_store_page_fault(SCAUSE_STORE_PAGE_FAULT));
        assert!(!is_store_page_fault(SCAUSE_LOAD_PAGE_FAULT));
        // Instruction page faults, interrupts, and an `ecall` are never
        // offered to the user-fault resolver.
        assert!(!is_load_page_fault(SCAUSE_INSTRUCTION_PAGE_FAULT));
        assert!(!is_store_page_fault(SCAUSE_INSTRUCTION_PAGE_FAULT));
        assert!(!is_load_page_fault(
            crate::trap::SCAUSE_INTERRUPT_BIT | SCAUSE_LOAD_PAGE_FAULT
        ));
        assert!(!is_store_page_fault(
            crate::trap::SCAUSE_INTERRUPT_BIT | SCAUSE_STORE_PAGE_FAULT
        ));
        assert!(!is_load_page_fault(8));
        assert!(!is_store_page_fault(8));
    }

    extern "C" fn host_user_fault_resolver(_stval: u64, _write: bool) -> bool {
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

    extern "C" fn host_fault_handler(_scause: u64, _stval: u64, _sepc: u64) -> ! {
        panic!("host test handler must never be invoked");
    }

    // Both the set-once and the round-trip assertions mutate the single
    // process-wide `FAULT_HANDLER` slot, so they live in one test: cargo
    // runs `#[test]`s in parallel threads and two of them clearing and
    // reinstalling the same static would race (no flaky
    // tests).
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
