//! TAIRiX kernel IRQ table and per-handle wait queue.
//!
//! Stage 4.D Item 2-tail. The kernel-side implementation of the
//! `irq_bind` / `irq_wait` `abi-v1` syscall pair gated by
//! `CAP_IRQ_BIND`. The user-visible contract this crate implements
//! is locked down in
//! [`docs/src/security/irq.md`](../../../docs/src/security/irq.md);
//! see that page for the wake-up ordering invariant and the
//! failure-mode table.
//!
//! # Design summary
//!
//! * **Pure-data wait queue.** This crate has zero threading
//!   concerns. The polling loop that drives `irq_wait` to completion
//!   lives in `kernel/core::syscalls::KernelSyscallHandlers::irq_wait`,
//!   which composes [`IrqTable::try_wait_step`] with
//!   `KernelArch::monotonic_ns` and `Scheduler::yield_current` —
//!   primitives that already exist. No new scheduler interface is
//!   introduced (no interface creep).
//! * **Mask-before-wake.** [`IrqTable::fire`] calls
//!   [`IrqController::mask`] *before* it sets the per-entry `ready`
//!   flag. The unit test `mask_is_observed_before_wake` exercises
//!   the ordering through a deterministic mock controller whose
//!   `mask` increments a counter that the test asserts against the
//!   counter snapshotted on `ready=true`.
//! * **Forgery defence in the table itself.** Every
//!   [`IrqTable::try_wait_step`] call re-verifies that the handle
//!   was minted for the calling `kernel/sec::TaskId` before any
//!   state transition. A forged handle returns
//!   [`WaitStep::NotFound`] which the syscall handler translates to
//!   `Errno::NotFound`; the dispatcher emits the standard
//!   `SyscallHandlerRejected` audit record.
//! * **No global mutable state.** The crate's [`IrqTable`] is
//!   instantiated exactly once by the kernel binary and held inside
//!   `KernelState`. Interior synchronisation is one
//!   `kernel/sync::RwLock` mirroring the `CapTable` lock-ordering
//!   policy, plus a set-once, lock-free `OnceCell` holding the
//!   optional [`IrqDispatchObserver`] (below) so the interrupt-context
//!   `fire` path reads it without a lock.
//! * **Interrupt-dispatch observer.** [`IrqTable::fire`] notifies a
//!   set-once [`IrqDispatchObserver`] at every interrupt arrival. The
//!   kernel installs one whose only job is to feed interrupt-arrival
//!   timing into the entropy pool (`lib/rng`); it is purely
//!   observational and never influences the mask-before-wake path.
//!
//! # Out of scope for this landing
//!
//! * **Trap glue.** The x86_64 IDT external-vector range, the
//!   per-vector assembly thunks, the LAPIC EOI prologue, and the
//!   MADT-driven IO-APIC redirection-entry programming are
//!   prerequisites for [`IrqTable::fire`] being called from a real
//!   hardware interrupt path; that work is the next session's lead
//!   per `.junie/next-session-prompt.md`. Until it lands, `fire`
//!   is exercised by host-side unit tests and (in production) by
//!   any synthetic test source the kernel binary chooses to wire.
//! * **Power-efficient blocking.** The `irq_wait` handler yields
//!   between polls rather than parking the caller. A future
//!   landing can replace the yield-cycle with `Scheduler::park` once
//!   the lost-wakeup race between `fire` and `park` is closed by a
//!   table-internal interlock; the current design is correct (no
//!   lost wakeups, timeouts honoured) at the cost of consuming
//!   scheduler quanta while waiting.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

mod error;
mod table;
mod wait;

pub use error::{IrqError, MaskError};
pub use table::{
    BindOutcome, FireOutcome, IrqController, IrqDispatchObserver, IrqEntry, IrqTable,
    ObserverAlreadyInstalled, ReleaseOutcome, UnsupportedController, WaitStep,
    UNSUPPORTED_CONTROLLER,
};
pub use wait::{block_until_ready, IrqWaitAbort, IrqWaiter, WaitOutcome};
