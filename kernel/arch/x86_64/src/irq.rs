//! x86_64 external-IRQ trap glue (Stage 4.D Item 2-tail.2).
//!
//! This module owns the architecture-specific surface for external
//! interrupts on x86_64:
//!
//! * The vector range reservation (`0x30..=0xFE`) and the per-vector
//!   asm thunk table published by `external_irq.s`.
//! * The lock-free [`Routing`] table mapping IDT vectors to
//!   architecture-neutral GSI line numbers.
//! * The Rust dispatcher invoked by the shared asm trampoline. The
//!   dispatcher looks up the GSI through [`Routing`], forwards to the
//!   architecture-neutral `rustos_kernel_irq::IrqTable::fire` via a
//!   one-shot-published callback ([`set_external_irq_dispatch`]), and
//!   writes the LAPIC EOI register before returning.
//! * The MADT IO-APIC discovery helper `discover_io_apics` consumed
//!   by the kernel binary's boot pipeline.
//!
//! # Mask-before-wake invariant
//!
//! Per `docs/src/security/irq.md` ("Wait semantics"), the kernel
//! requires every controller to mask the line before signalling
//! `ready = true`. The Rust dispatcher in this module **does not**
//! perform the mask write itself — it calls
//! `rustos_kernel_irq::IrqTable::fire`, which in turn invokes the
//! installed controller's `mask` *before* setting `ready`. The
//! production controller (`IoApicController` in
//! `kernel/rustos-kernel`) issues a volatile, fenced write to the
//! IO-APIC's redirection-entry mask bit. The
//! `mask_is_observed_before_wake` regression test in
//! `kernel/irq` and the `ioapic_controller_mask_before_wake`
//! kernel-binary host test pin the contract.
//!
//! # No global mutable state
//!
//! The [`Routing`] table and the dispatcher slot
//! ([`set_external_irq_dispatch`]) are set-once at boot. They are
//! backed by atomics so they can be read concurrently from every
//! CPU's trap path without taking a lock. Writers fail-closed on a
//! second publish (one-shot publish, no mutable
//! static).

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use crate::interrupts::SavedRegs;

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use crate::preempt::{LAPIC_BASE_PHYS, LAPIC_EOI_OFFSET};

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use rustos_abi::MsiMessage;

mod routing;

pub use routing::{Routing, RoutingError};

// --- Vector range -------------------------------------------------

/// First IDT vector reserved for external IRQs.
///
/// `0x30` sits above the architectural exception range (0x00..=0x1F)
/// and the LAPIC-internal vectors (`TIMER_VECTOR = 0x20`,
/// LINT0/LINT1, spurious). The Intel SDM Vol 3A §6.2 reserves 0..32
/// for the CPU; the kernel claims 0x20..=0x2F for LAPIC-internal
/// sources (timer, IPIs, spurious) and 0x30..=0xFE for external
/// IRQs delivered through the IO-APIC.
pub const EXTERNAL_VECTOR_FIRST: u8 = 0x30;

/// Last IDT vector reserved for external IRQs (inclusive).
///
/// `0xFF` is the architectural spurious-interrupt vector and must
/// never be wired to a Rust handler — Intel SDM Vol 3A §11.9.
pub const EXTERNAL_VECTOR_LAST: u8 = 0xFE;

/// Number of reserved external-IRQ vectors.
pub const EXTERNAL_VECTOR_COUNT: usize =
    (EXTERNAL_VECTOR_LAST as usize) - (EXTERNAL_VECTOR_FIRST as usize) + 1;

// --- Per-vector stub table ----------------------------------------

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
extern "C" {
    /// Per-vector stub address table published by `external_irq.s`.
    ///
    /// Indexed by `vector - EXTERNAL_VECTOR_FIRST`. Each entry is the
    /// linear address of the per-vector stub the IDT delivers to.
    /// `'static` immutable data in `.rodata`; safe to read without
    /// synchronisation.
    static rustos_arch_x86_64_external_irq_table: [usize; EXTERNAL_VECTOR_COUNT];
}

/// Linear address of the per-vector ISR stub for `vector`.
///
/// Returns `None` if `vector` is outside the reserved external-IRQ
/// range — the IDT installer in the kernel binary fail-closes in
/// that case.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[must_use]
pub fn external_isr_addr(vector: u8) -> Option<u64> {
    if !(EXTERNAL_VECTOR_FIRST..=EXTERNAL_VECTOR_LAST).contains(&vector) {
        return None;
    }
    let idx = (vector - EXTERNAL_VECTOR_FIRST) as usize;
    // SAFETY: `rustos_arch_x86_64_external_irq_table` is a
    // `'static`-lifetime `.rodata` array of exactly
    // `EXTERNAL_VECTOR_COUNT` entries published by
    // `external_irq.s`; the `idx < EXTERNAL_VECTOR_COUNT` bound is
    // proved by the range check immediately above.
    let addr = unsafe { rustos_arch_x86_64_external_irq_table[idx] };
    Some(addr as u64)
}

/// Host-build stub: returns `None` because the asm table only exists
/// on the freestanding target. Tests that need to exercise the
/// routing surface use the [`Routing`] type directly.
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
#[must_use]
pub fn external_isr_addr(_vector: u8) -> Option<u64> {
    None
}

// --- Dispatcher slot ----------------------------------------------

/// Slot holding the installed external-IRQ dispatcher, as a raw
/// function pointer packed into a `usize`.
///
/// The kernel binary installs the production dispatcher (which
/// forwards to `rustos_kernel_irq::IrqTable::fire`) exactly once
/// during boot via [`set_external_irq_dispatch`]. The asm trampoline
/// calls `rustos_arch_x86_64_external_irq_dispatch` on every
/// delivery; that Rust function reads this slot, looks up the GSI
/// through [`global_routing`], forwards, and writes EOI.
static EXTERNAL_IRQ_DISPATCH_FN: AtomicUsize = AtomicUsize::new(0);

/// Signature of the installed external-IRQ dispatcher.
///
/// `vector` is the IDT vector the trap fired on; the dispatcher is
/// responsible for translating it to a GSI through [`global_routing`]
/// and invoking `rustos_kernel_irq::IrqTable::fire` before
/// returning. The dispatcher must be safe to invoke from interrupt
/// context (interrupts disabled, no allocation, no scheduler reentry).
pub type ExternalIrqDispatchFn = extern "C" fn(vector: u8);

/// Install the production external-IRQ dispatcher.
///
/// Returns [`SetDispatchError::AlreadyInstalled`] on the second
/// publish (one-shot publish).
pub fn set_external_irq_dispatch(cb: ExternalIrqDispatchFn) -> Result<(), SetDispatchError> {
    let raw = cb as usize;
    EXTERNAL_IRQ_DISPATCH_FN
        .compare_exchange(0, raw, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| SetDispatchError::AlreadyInstalled)
}

/// Failure modes of [`set_external_irq_dispatch`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SetDispatchError {
    /// A dispatcher was already published. The slot is set-once per
    /// boot.
    AlreadyInstalled,
}

/// Test-only inspection of the installed dispatcher.
///
/// Returns the installed function pointer's address, or `0` when no
/// dispatcher has been installed yet. Reserved for the
/// `set_external_irq_dispatch_*` host tests.
#[must_use]
pub fn external_irq_dispatch_addr() -> usize {
    EXTERNAL_IRQ_DISPATCH_FN.load(Ordering::Acquire)
}

#[cfg(test)]
pub(crate) fn clear_external_irq_dispatch_for_tests() {
    // Test-only helper so back-to-back host tests can re-install a
    // dispatcher. — permitted in tests; production
    // code never clears the slot.
    EXTERNAL_IRQ_DISPATCH_FN.store(0, Ordering::Release);
}

// --- Global routing slot ------------------------------------------

/// Sentinel meaning "no GSI bound to this vector".
const GSI_UNMAPPED: u32 = u32::MAX;

/// Boot-time-populated, read-only-after-init routing table.
///
/// Backed by an array of [`AtomicU32`]; one slot per reserved
/// external vector. `u32::MAX` is the unmapped sentinel.
/// Once the kernel binary's `Phase::Irq` step completes, the table
/// is read-only (one-shot publish).
static GLOBAL_ROUTING: [AtomicU32; EXTERNAL_VECTOR_COUNT] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const Z: AtomicU32 = AtomicU32::new(GSI_UNMAPPED);
    [Z; EXTERNAL_VECTOR_COUNT]
};

/// Borrow the boot-time-published [`Routing`] table.
#[must_use]
pub fn global_routing() -> &'static Routing {
    // SAFETY: `Routing` is `#[repr(transparent)]` over the atomic
    // array `GLOBAL_ROUTING`. The cast preserves provenance.
    unsafe { &*core::ptr::addr_of!(GLOBAL_ROUTING).cast::<Routing>() }
}

#[cfg(test)]
#[allow(dead_code)] // Reserved for future host tests that exercise GLOBAL_ROUTING.
pub(crate) fn clear_routing_for_tests() {
    for slot in &GLOBAL_ROUTING {
        slot.store(GSI_UNMAPPED, Ordering::Release);
    }
}

// --- MSI message construction -------------------------------------

/// Build the x86 local-APIC MSI message that delivers `vector` to the
/// CPU whose local-APIC ID is `destination`.
///
/// This is the architecture half of the PCI MSI-X routing seam
/// ([`rustos_abi::MsixBus`]): the kernel picks a free external vector
/// (`0x30..=0xFE`) and a destination CPU, this function encodes the
/// Intel-defined message, and the PCI bus driver writes it into the
/// device's MSI-X table.
///
/// The encoding uses **physical** destination mode, **fixed** delivery,
/// and **edge** trigger (Intel SDM Vol 3A §11.11):
///
/// * Address — the LAPIC message-address format: the fixed `0xFEE`
///   prefix (here [`LAPIC_BASE_PHYS`](crate::preempt::LAPIC_BASE_PHYS))
///   with the destination APIC ID in bits 19..12 and the redirection-
///   hint / destination-mode bits clear.
/// * Data — the low byte carries the vector; the delivery-mode,
///   level, and trigger-mode bits are all zero.
#[must_use]
pub fn msi_message(vector: u8, destination: u8) -> MsiMessage {
    let address = crate::preempt::LAPIC_BASE_PHYS | (u64::from(destination) << 12);
    MsiMessage {
        address,
        data: u32::from(vector),
    }
}

// --- Rust trampoline called from the asm thunk -------------------

/// Rust entry point invoked by the asm trampoline.
///
/// `_regs` is the `SavedRegs` block the trampoline pushed; the
/// dispatcher does not currently consult it (the kernel-neutral
/// fire path needs only the GSI). It is kept in the signature so a
/// future commit can inspect the trap frame without touching the
/// ISR ABI (no interface creep, extend through
/// the existing pointer).
///
/// Behaviour:
///   1. Truncates `vector` to `u8` and (if out of range) signals a
///      spurious delivery by writing EOI and returning — the IDT
///      should never route a non-external vector here, so the check
///      is belt-and-braces.
///   2. Calls the installed [`ExternalIrqDispatchFn`] with the
///      truncated vector. The dispatcher forwards to
///      `rustos_kernel_irq::IrqTable::fire`.
///   3. Writes `0` to the LAPIC EOI register releasing the
///      in-service bit (Intel SDM Vol 3A §11.8.5). EOI is performed
///      *after* the dispatcher because the dispatcher must observe
///      a consistent mask state through [`global_routing`] before
///      the next delivery on the same vector can stack.
///
/// # Safety
///
/// Only callable from the asm trampoline. Invoking it from
/// arbitrary Rust would corrupt the LAPIC's TPR-arbitration state
/// because the EOI write below assumes the in-service bit is set.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[no_mangle]
unsafe extern "C" fn rustos_arch_x86_64_external_irq_dispatch(_regs: *mut SavedRegs, vector: u64) {
    // The asm trampoline pushes a u64 that fits in u8 (every value
    // is ≤ 0xFE by construction). Documented narrowing.
    #[allow(clippy::cast_possible_truncation)]
    let vector_u8 = vector as u8;

    if (EXTERNAL_VECTOR_FIRST..=EXTERNAL_VECTOR_LAST).contains(&vector_u8) {
        let raw = EXTERNAL_IRQ_DISPATCH_FN.load(Ordering::Acquire);
        if raw != 0 {
            // SAFETY: every store into the slot rounds-trips a valid
            // `ExternalIrqDispatchFn` pointer through
            // `set_external_irq_dispatch`. Function pointers are
            // `usize`-sized; the transmute is lossless.
            let cb: ExternalIrqDispatchFn =
                unsafe { core::mem::transmute::<usize, ExternalIrqDispatchFn>(raw) };
            cb(vector_u8);
        }
        // If no dispatcher is installed we fall through to EOI. A
        // spurious IRQ before the production dispatcher is published
        // is impossible in practice (the boot pipeline installs the
        // dispatcher before unmasking any line) but the silent-EOI
        // path keeps the LAPIC out of stuck-in-service.
    }

    // SAFETY: LAPIC EOI register at the architecturally-fixed offset.
    // Writing `0` is the documented "end-of-interrupt" sequence.
    unsafe {
        let eoi = (LAPIC_BASE_PHYS + LAPIC_EOI_OFFSET as u64) as *mut u32;
        core::ptr::write_volatile(eoi, 0);
    }
}

// --- Host tests ----------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_range_constants_match_intel_sdm() {
        assert_eq!(EXTERNAL_VECTOR_FIRST, 0x30);
        assert_eq!(EXTERNAL_VECTOR_LAST, 0xFE);
        assert_eq!(EXTERNAL_VECTOR_COUNT, 207);
    }

    #[test]
    fn msi_message_encodes_destination_and_vector() {
        // Vector 0x41 to APIC ID 0: address is the bare LAPIC base,
        // data is the vector.
        let m = msi_message(0x41, 0);
        assert_eq!(m.address, 0xFEE0_0000);
        assert_eq!(m.data, 0x41);
    }

    #[test]
    fn msi_message_places_destination_in_bits_19_12() {
        // APIC ID 0xAB lands in bits 19..12 of the message address;
        // the fixed 0xFEE prefix is preserved and the low 12 bits stay
        // clear (redirection hint / destination mode == 0).
        let m = msi_message(0x30, 0xAB);
        assert_eq!(m.address, 0xFEE0_0000 | (0xAB << 12));
        assert_eq!(m.address & 0xFFF, 0);
        assert_eq!(m.data, 0x30);
    }

    #[test]
    fn external_isr_addr_returns_none_on_host() {
        // The asm table is only emitted on the freestanding target;
        // the host build returns None so test scaffolding stays
        // deterministic.
        assert!(external_isr_addr(0x30).is_none());
        assert!(external_isr_addr(0xFE).is_none());
    }

    #[test]
    fn external_isr_addr_rejects_vectors_outside_range() {
        assert!(external_isr_addr(0x20).is_none());
        assert!(external_isr_addr(0x2F).is_none());
        assert!(external_isr_addr(0xFF).is_none());
    }

    extern "C" fn host_test_dispatcher_cb(_vector: u8) {}

    #[test]
    fn set_external_irq_dispatch_fails_closed_on_second_install() {
        clear_external_irq_dispatch_for_tests();
        set_external_irq_dispatch(host_test_dispatcher_cb).expect("first install succeeds");
        assert_eq!(
            set_external_irq_dispatch(host_test_dispatcher_cb),
            Err(SetDispatchError::AlreadyInstalled),
        );
        clear_external_irq_dispatch_for_tests();
    }

    #[test]
    fn external_irq_dispatch_addr_round_trips_installed_fn() {
        clear_external_irq_dispatch_for_tests();
        set_external_irq_dispatch(host_test_dispatcher_cb).expect("install");
        assert_eq!(
            external_irq_dispatch_addr(),
            host_test_dispatcher_cb as *const () as usize,
        );
        clear_external_irq_dispatch_for_tests();
    }
}
