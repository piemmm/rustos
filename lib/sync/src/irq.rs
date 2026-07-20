//! Interrupt-control abstraction used by [`IrqSafeSpinLock`].
//!
//! A spinlock that may be acquired from interrupt context must disable
//! interrupts on the current CPU before taking the lock and re-enable them
//! to the previous state on release. The actual primitives that disable
//! interrupts are architecture-specific (`cli`/`sti` on x86_64,
//! `msr daifset` on `AArch64`, `csrrci sstatus` on RISC-V, etc.), so the
//! sync crate exposes a trait that architecture crates implement and the
//! generic spinlock plugs in.
//!
//! The default plug, [`NopInterruptControl`], is a no-op that is **only**
//! valid on host (hosted) test builds and inside the `wasm32` port where
//! the runtime never raises asynchronous interrupts. Kernel code on real
//! hardware MUST select a real implementation by type-parameterising
//! [`IrqSafeSpinLock`](crate::IrqSafeSpinLock) with the architecture's
//! type.
//!
//! # Standing rule: any ISR-shared lock must be IRQ-safe
//!
//! TAIRiX runs syscall bodies — and every in-kernel task — with device
//! interrupts **enabled**, so an interrupt service routine can fire on the
//! same CPU while a syscall-reachable code path holds a lock. If that lock
//! were a plain [`SpinLock`](crate::SpinLock) and the ISR took it too, the
//! ISR would spin forever on a CPU whose interrupted task already holds it
//! — a single-CPU self-deadlock. Therefore, for **any** lock that is
//! shared between a driver's (or subsystem's) ISR and its
//! syscall-reachable body, one of the following MUST hold:
//!
//! * the shared lock is an [`IrqSafeSpinLock`](crate::IrqSafeSpinLock),
//!   which masks interrupts on the current CPU for the duration of the
//!   hold (the console UART receive ring's `UART_RX_GATE` is the worked
//!   example), **or**
//! * the ISR side is entirely lock-free and only *flags* deferred work
//!   (e.g. `WaitQueue::request_wake` sets one atomic), with the real
//!   lock-taking wake/drain performed later in dispatcher context (the
//!   `kernel/core` deferred-drain model).
//!
//! A plain-`SpinLock` structure shared with an ISR and *not* gated this
//! way is a defect — it is exactly the deadlock the interruptible-syscall
//! design would otherwise introduce. New drivers are held to this rule in
//! review.
//!
//! [`IrqSafeSpinLock`]: crate::IrqSafeSpinLock

use core::marker::PhantomData;

/// Architecture-supplied saved interrupt state.
///
/// Implementations must keep this type cheap to copy (typically a
/// `usize`-sized bitfield) because every `lock()`/`unlock()` pair on an
/// IRQ-safe spinlock saves and restores one of these.
pub trait IrqState: Copy + 'static {}

/// Architecture-supplied interrupt enable/disable primitives.
///
/// # Safety
///
/// Implementors must guarantee:
///
/// 1. [`disable`](Self::disable) returns the previous interrupt-enable
///    state and atomically masks interrupts on the current CPU.
/// 2. [`restore`](Self::restore) restores exactly the state that
///    [`disable`](Self::disable) returned, with no ordering surprises.
/// 3. The pair is reentrant: calling `disable` while interrupts are
///    already disabled returns a "disabled" state that, when restored,
///    leaves interrupts disabled.
pub unsafe trait InterruptControl {
    /// Saved-state type returned by [`disable`](Self::disable).
    type State: IrqState;

    /// Disable interrupts on the current CPU and return the previous state.
    fn disable() -> Self::State;

    /// Restore the previous interrupt state captured by an earlier
    /// [`disable`](Self::disable) call.
    ///
    /// # Safety
    ///
    /// The caller must pass a state that was produced by a matching
    /// `disable` on the *same* CPU and has not yet been restored.
    unsafe fn restore(state: Self::State);
}

/// A no-op [`IrqState`].
///
/// Used by [`NopInterruptControl`]. Carries no data — the underlying
/// platform either does not have asynchronous interrupts or the caller
/// has guaranteed (e.g. via running the test on a host) that no interrupt
/// can reach the spinlock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NopIrqState(PhantomData<()>);

impl IrqState for NopIrqState {}

/// No-op [`InterruptControl`] implementation.
///
/// `NopInterruptControl` is appropriate **only**:
/// - on hosted (`cargo test`) builds, where there are no kernel-style
///   interrupts in the first place;
/// - inside the `wasm32` port, which is single-threaded and cooperatively
///   scheduled;
/// - inside unit tests of other kernel crates that exercise lock APIs
///   without genuinely racing against interrupts.
///
/// Selecting this implementation for a real hardware port is a defect.
#[derive(Clone, Copy, Debug, Default)]
pub struct NopInterruptControl;

// SAFETY: `NopInterruptControl::disable` returns a unit state and
// `restore` is the identity. The trait obligations (1)–(3) above are
// trivially satisfied because the implementation neither observes nor
// mutates any real CPU state.
unsafe impl InterruptControl for NopInterruptControl {
    type State = NopIrqState;

    #[inline]
    fn disable() -> Self::State {
        NopIrqState(PhantomData)
    }

    #[inline]
    unsafe fn restore(_state: Self::State) {}
}
