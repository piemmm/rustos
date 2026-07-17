//! Interrupt-controller and interrupt-entry surface of the Arch HAL
//! ("interrupt entry/exit").
//!
//! Every architecture routes external device interrupts through a
//! programmable interrupt controller and a per-interrupt
//! prologue/epilogue. The controllers differ in register layout — the
//! x86_64 IO-APIC, the aarch64 GICv2, the riscv64 PLIC — but the kernel
//! needs the *same* two operations from each: hold a line masked at the
//! controller (the load-bearing mask-before-wake primitive of the
//! user-space IRQ contract, `docs/src/security/irq.md`), and, for a
//! claim-based controller, run the claim → handle → complete handshake
//! that drains a pending interrupt. The charter makes the architecture surface
//! a closed set of traits on the HAL; this module is the
//! "interrupt entry/exit" member of that set, so the controller logic
//! lives behind one vocabulary instead of being re-described at every
//! call site.
//!
//! # What lives here
//!
//! * [`IrqController`] — the line-masking surface every port with a
//!   programmable controller implements. It is the HAL-level companion
//!   of the consumer-side `tairix_kernel_irq::IrqController` (which the
//!   IRQ table calls through during a wake): a port implements this HAL
//!   trait over its real controller, and the kernel binary bridges the
//!   two (the arch port owns no `kernel/irq`
//!   dependency).
//! * [`InterruptEntry`] — the claim/complete prologue/epilogue a
//!   *claim-based* controller (PLIC, GIC) exposes. A **vectored** port
//!   (x86_64, whose IDT vector already identifies the source and whose
//!   end-of-interrupt is a single LAPIC write independent of the line)
//!   has no claim register, so it honestly does not implement this trait
//!   — the same "declare the absence, never fake it" discipline the
//!   side-channel and memory-tagging slices follow.
//! * [`conformance`] — the conformance verticals: a host-run
//!   [`conformance::run_controller`] mask/unmask round-trip + fail-closed
//!   check every controller-bearing port runs over its handle, and a
//!   [`conformance::run_entry`] claim/complete drain check every
//!   claim-based port runs over its handle. They are driven per port
//!   rather than folded into [`crate::conformance::run_all`] because the
//!   controller check needs a port-specific valid/invalid line pair and
//!   the entry check is implemented by only a subset of ports — the same
//!   reason [`crate::percpu::conformance::run_isolation`] is a separate
//!   vertical.

/// Failure mode of an [`IrqController`] operation.
///
/// The controller validates every line before touching a register and
/// fails closed: an out-of-range line is rejected,
/// never silently masking or unmasking an unrelated source.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum IrqControlError {
    /// `line` is outside the controller's addressable range (for
    /// example PLIC source `0`, or a GSI/INTID above the highest
    /// configured line).
    OutOfRange,
}

/// The interrupt-controller line-masking handle an architecture port
/// exposes.
///
/// The kernel holds a line masked at the controller while a driver
/// drains the device, then unmasks it. [`Self::mask`] is the primitive
/// the user-space IRQ contract requires to complete *before* a waiter's
/// `ready` flag is observed (`docs/src/security/irq.md`); a port's
/// implementation pairs the masking register write with a
/// [`core::sync::atomic::fence`] so the masked state is globally visible
/// before the wake.
///
/// Implementations must be [`Send`] + [`Sync`]: the kernel reaches the
/// controller from every CPU's interrupt path.
pub trait IrqController: Send + Sync {
    /// Mask `line` at the controller so it can no longer deliver.
    ///
    /// Idempotent: masking an already-masked line succeeds. Must publish
    /// the masked state before returning (the port pairs the register
    /// write with a release/`SeqCst` fence).
    ///
    /// # Errors
    ///
    /// [`IrqControlError::OutOfRange`] if `line` is outside the
    /// controller's addressable range. The call never panics on a
    /// stray line.
    fn mask(&self, line: u32) -> Result<(), IrqControlError>;

    /// Clear the mask on `line`, restoring delivery.
    ///
    /// Symmetric counterpart of [`Self::mask`]. Idempotent.
    ///
    /// # Errors
    ///
    /// [`IrqControlError::OutOfRange`] if `line` is outside the
    /// controller's addressable range.
    fn unmask(&self, line: u32) -> Result<(), IrqControlError>;
}

/// The claim/complete interrupt prologue/epilogue a *claim-based*
/// controller exposes.
///
/// On a claim-based controller (riscv64 PLIC, aarch64 GICv2) the
/// interrupt handler reads a claim register to learn which source is
/// active, services it, then writes a completion register to deactivate
/// it. This trait is that handshake. A **vectored** port (x86_64) learns
/// the source from the IDT vector the CPU dispatched to and signals
/// end-of-interrupt with a single LAPIC write that names no line, so it
/// does not implement this trait — the claim register has no analogue
/// there, and inventing one would be a fake primitive.
///
/// Implementations must be [`Send`] + [`Sync`].
pub trait InterruptEntry: Send + Sync {
    /// Claim the highest-priority pending interrupt, returning its line,
    /// or [`None`] when nothing is pending (a spurious read).
    ///
    /// Claiming an interrupt activates it at the controller; the caller
    /// must subsequently [`Self::complete`] the same line. Repeated
    /// calls with nothing pending return [`None`] (they never wedge or
    /// invent a line).
    fn claim(&self) -> Option<u32>;

    /// Signal completion of `line`, deactivating it at the controller.
    ///
    /// Called with a line previously returned by [`Self::claim`]. A
    /// completion for a line that was not active is dropped
    /// best-effort, never a panic.
    fn complete(&self, line: u32);
}

/// The interrupt-controller / interrupt-entry conformance
/// verticals.
///
/// Each controller-bearing port runs [`conformance::run_controller`]
/// against its [`IrqController`] handle, and each claim-based port runs
/// [`conformance::run_entry`] against its [`InterruptEntry`] handle.
/// Both are host-run and name only the traits, exactly like the sibling
/// [`crate::percpu::conformance`] and [`crate::memtag::conformance`]
/// verticals. They are deliberately *not* folded into
/// [`crate::conformance::run_all`]: the controller check needs a
/// port-specific valid/invalid `line` pair (which the handle cannot
/// self-describe without interface creep) and the
/// entry check is implemented by only a subset of ports.
pub mod conformance {
    use super::{InterruptEntry, IrqControlError, IrqController};

    /// Upper bound on claim/complete iterations before the drain is
    /// declared non-terminating. A faithful controller drains a finite
    /// pending set well within this; a broken one that re-claims the
    /// same line forever trips it.
    const DRAIN_LIMIT: usize = 4096;

    /// Run the entire [`IrqController`] conformance suite against
    /// `controller`.
    ///
    /// `valid_line` must be an addressable line on this controller and
    /// `invalid_line` one outside its range; each port passes the pair
    /// matching its own controller (PLIC source range, GSI range, GIC
    /// INTID range).
    ///
    /// # Panics
    ///
    /// Panics (failing the test) if masking is not fail-closed
    /// (`invalid_line` is accepted) or not idempotent/round-tripping on
    /// `valid_line`.
    pub fn run_controller<C: IrqController + ?Sized>(
        controller: &C,
        valid_line: u32,
        invalid_line: u32,
    ) {
        valid_line_round_trips(controller, valid_line);
        invalid_line_fails_closed(controller, invalid_line);
    }

    /// A valid line masks, unmasks, and re-masks without error: the
    /// operations are total and idempotent on an addressable line.
    fn valid_line_round_trips<C: IrqController + ?Sized>(controller: &C, line: u32) {
        assert_eq!(
            controller.mask(line),
            Ok(()),
            "masking an addressable line must succeed (line {line})"
        );
        assert_eq!(
            controller.mask(line),
            Ok(()),
            "masking must be idempotent (line {line})"
        );
        assert_eq!(
            controller.unmask(line),
            Ok(()),
            "unmasking an addressable line must succeed (line {line})"
        );
        assert_eq!(
            controller.unmask(line),
            Ok(()),
            "unmasking must be idempotent (line {line})"
        );
        assert_eq!(
            controller.mask(line),
            Ok(()),
            "a line must re-mask after being unmasked (line {line})"
        );
    }

    /// An out-of-range line is rejected by both operations and never
    /// panics: the controller fails closed.
    fn invalid_line_fails_closed<C: IrqController + ?Sized>(controller: &C, line: u32) {
        assert_eq!(
            controller.mask(line),
            Err(IrqControlError::OutOfRange),
            "masking an out-of-range line must fail closed (line {line})"
        );
        assert_eq!(
            controller.unmask(line),
            Err(IrqControlError::OutOfRange),
            "unmasking an out-of-range line must fail closed (line {line})"
        );
    }

    /// Run the entire [`InterruptEntry`] conformance suite against
    /// `entry`.
    ///
    /// Drains every pending interrupt through the claim → complete
    /// handshake, then asserts the empty controller reports nothing
    /// pending.
    ///
    /// # Panics
    ///
    /// Panics (failing the test) if the claim/complete handshake does
    /// not terminate (a line that re-claims forever), or if a fully
    /// drained controller still reports a pending interrupt.
    pub fn run_entry<E: InterruptEntry + ?Sized>(entry: &E) {
        let mut drained = 0usize;
        while let Some(line) = entry.claim() {
            entry.complete(line);
            drained += 1;
            assert!(
                drained < DRAIN_LIMIT,
                "claim/complete must terminate; a faithful controller drains its \
                 pending set, it does not re-claim the same line forever"
            );
        }
        assert!(
            entry.claim().is_none(),
            "a fully drained controller must report no pending interrupt"
        );
    }

    #[cfg(test)]
    mod tests {
        use super::super::{InterruptEntry, IrqControlError, IrqController};
        use super::{run_controller, run_entry};
        use core::sync::atomic::{AtomicU32, Ordering};

        /// A faithful host double: a controller addressing lines
        /// `1..=max`, modelling the per-line mask bit in a word.
        struct CellController {
            max: u32,
            masked_bits: AtomicU32,
        }

        impl CellController {
            fn new(max: u32) -> Self {
                Self {
                    max,
                    masked_bits: AtomicU32::new(0),
                }
            }
            fn in_range(&self, line: u32) -> bool {
                line != 0 && line <= self.max
            }
        }

        impl IrqController for CellController {
            fn mask(&self, line: u32) -> Result<(), IrqControlError> {
                if !self.in_range(line) {
                    return Err(IrqControlError::OutOfRange);
                }
                self.masked_bits.fetch_or(1 << line, Ordering::Relaxed);
                Ok(())
            }
            fn unmask(&self, line: u32) -> Result<(), IrqControlError> {
                if !self.in_range(line) {
                    return Err(IrqControlError::OutOfRange);
                }
                self.masked_bits.fetch_and(!(1 << line), Ordering::Relaxed);
                Ok(())
            }
        }

        #[test]
        fn controller_suite_accepts_a_faithful_cell() {
            let controller = CellController::new(8);
            run_controller(&controller, 3, 9);
            let dynamic: &dyn IrqController = &controller;
            run_controller(dynamic, 3, 9);
        }

        /// A controller that accepts every line — including the
        /// out-of-range one — is not fail-closed and must be rejected.
        struct PromiscuousController;

        impl IrqController for PromiscuousController {
            fn mask(&self, _line: u32) -> Result<(), IrqControlError> {
                Ok(())
            }
            fn unmask(&self, _line: u32) -> Result<(), IrqControlError> {
                Ok(())
            }
        }

        #[test]
        #[should_panic(expected = "must fail closed")]
        fn controller_suite_rejects_a_promiscuous_controller() {
            run_controller(&PromiscuousController, 3, 9);
        }

        /// A faithful claim-based entry: a fixed pending set drained one
        /// line per claim.
        struct QueueEntry {
            remaining: AtomicU32,
        }

        impl QueueEntry {
            fn new(pending: u32) -> Self {
                Self {
                    remaining: AtomicU32::new(pending),
                }
            }
        }

        impl InterruptEntry for QueueEntry {
            fn claim(&self) -> Option<u32> {
                loop {
                    let n = self.remaining.load(Ordering::Relaxed);
                    if n == 0 {
                        return None;
                    }
                    if self
                        .remaining
                        .compare_exchange(n, n - 1, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                    {
                        return Some(n);
                    }
                }
            }
            fn complete(&self, _line: u32) {}
        }

        #[test]
        fn entry_suite_drains_a_finite_pending_set() {
            run_entry(&QueueEntry::new(3));
            run_entry(&QueueEntry::new(0));
            let dynamic: &dyn InterruptEntry = &QueueEntry::new(2);
            run_entry(dynamic);
        }

        /// A broken entry that always re-claims the same line never
        /// drains; the suite must catch the non-termination.
        struct StuckEntry;

        impl InterruptEntry for StuckEntry {
            fn claim(&self) -> Option<u32> {
                Some(7)
            }
            fn complete(&self, _line: u32) {}
        }

        #[test]
        #[should_panic(expected = "must terminate")]
        fn entry_suite_rejects_a_non_terminating_claim() {
            run_entry(&StuckEntry);
        }
    }
}
