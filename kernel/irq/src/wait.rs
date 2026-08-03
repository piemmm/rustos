//! Shared cooperative blocking loop for [`IrqTable`] waiters.
//!
//! Two independent in-kernel call sites need to block a task until a
//! bound IRQ line fires:
//!
//! * the `irq_wait` `abi-v1` syscall handler in
//!   `kernel/core::syscalls`, and
//! * the in-kernel `KernelVirtioHost::notify_wait` path in
//!   `drivers/bus/virtio` (Stage 4.D Item 2-tail.3).
//!
//! Both run the same loop: compute a deadline once, poll
//! [`IrqTable::try_wait_step`], and cooperatively yield between polls
//! until the line fires, the deadline elapses, or the binding
//! disappears. To avoid two copies of that loop,
//! the loop lives here exactly once and is parameterised over the
//! clock + yield seam through the [`IrqWaiter`] trait.
//!
//! `kernel/irq` stays free of any scheduler or architecture
//! dependency: the trait is the inversion point, and each caller
//! supplies its own implementation (the syscall handler wraps
//! `Scheduler::yield_current` + `KernelArch::monotonic_ns`; the
//! virtio host wraps the kernel-binary's equivalent seam).

use tairix_abi::IrqHandle;
use tairix_kernel_sec::TaskId;

use crate::table::{IrqTable, WaitStep};

/// Clock + cooperative-yield seam the [`block_until_ready`] loop
/// drives.
///
/// The two methods are the only kernel primitives the blocking loop
/// needs; keeping them behind a trait lets `kernel/irq` remain
/// `no_std` and free of scheduler / architecture dependencies
/// (no interface creep, the surface is exactly two
/// methods).
pub trait IrqWaiter {
    /// Current value of the kernel monotonic clock, in nanoseconds,
    /// on the CPU the waiting task is executing on.
    ///
    /// Must be non-decreasing across calls within a single
    /// [`block_until_ready`] invocation; the loop computes its
    /// deadline from the first reading and compares every subsequent
    /// reading against it, so a clock that went backwards would
    /// extend the wait rather than corrupt the table.
    fn now_ns(&self) -> u64;

    /// Suspend the calling task until it should re-poll the line, or
    /// until the absolute monotonic `deadline_ns` the loop is bounded by.
    ///
    /// Called once per poll iteration that observes neither a fire
    /// nor a timeout. How it suspends is the implementation's choice:
    /// the `irq_wait` syscall path *parks* off the run queue (woken by
    /// the device-IRQ dispatch path's wake or the timed sweep —
    /// , no busy yield), while the in-kernel kthread
    /// path suspends through its own race-free wait (a `wfi` park on
    /// metal, a cooperative yield under the QEMU verticals). Returning
    /// [`Ok`] re-enters the loop; returning [`Err`] aborts the wait with
    /// the supplied [`IrqWaitAbort`] reason (e.g. the task can no longer
    /// be scheduled).
    ///
    /// `deadline_ns` is the same bound [`block_until_ready`] polls
    /// against, handed to the park so a parking implementation can
    /// register it with a timed wake source and be released even when the
    /// line never fires at all. A park that is *not* told the deadline can
    /// only be woken by the line itself, which turns a lost or coalesced
    /// completion interrupt into a task parked forever — and a task parked
    /// forever inside a device operation holds that device's lock forever,
    /// wedging every other consumer of the same hardware. Passing the
    /// bound through here is what makes a bounded wait bounded in fact and
    /// not merely in intent. [`u64::MAX`] means the caller asked for no
    /// deadline (the `irq_wait` / `waitset_wait` convention), so the park
    /// waits on the line alone.
    ///
    /// # Errors
    ///
    /// Implementation-defined; see [`IrqWaitAbort`].
    fn yield_now(&self, deadline_ns: u64) -> Result<(), IrqWaitAbort>;
}

/// Reason a cooperative [`IrqWaiter::yield_now`] aborted the wait.
///
/// The blocking loop has no scheduler vocabulary of its own; each
/// caller maps these reasons onto its own error surface (the syscall
/// handler maps [`Self::TaskVanished`] to `Errno::NotFound`,
/// [`Self::Interrupted`] to `Errno::Interrupted`, and
/// [`Self::SchedulerError`] to `Errno::OutOfRange`).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum IrqWaitAbort {
    /// The waiting task can no longer be scheduled — it has been torn
    /// down between two polls. Fail closed.
    TaskVanished,
    /// The waiting task has a termination pending against it: the wait
    /// unwinds so the task can exit at its syscall boundary instead of
    /// sleeping on as an unkillable waiter. The aborted result never
    /// reaches user space — the kernel lands the pending kill first.
    Interrupted,
    /// The yield seam refused for any other reason. Defensive; not
    /// expected during normal operation.
    SchedulerError,
}

/// Terminal outcome of [`block_until_ready`].
///
/// Mirrors the non-`Continue` arms of [`WaitStep`] plus the
/// yield-abort case the loop can encounter that a single
/// [`IrqTable::try_wait_step`] poll cannot produce.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WaitOutcome {
    /// The bound line fired; the ready flag has been consumed.
    Ready,
    /// `now_ns >= deadline_ns` was reached without a fire.
    TimedOut,
    /// The handle was forged, or its binding was released while the
    /// loop was waiting.
    NotFound,
    /// The bound line was quarantined by the runaway-interrupt safety net
    /// (it fired past its rate budget), so the kernel disabled it. A
    /// terminal, fail-closed outcome: the caller surfaces an error rather
    /// than re-arming, which would immediately re-storm the line.
    Quarantined,
    /// A cooperative yield aborted the wait before any of the above.
    Aborted(IrqWaitAbort),
}

/// Block the calling task on `handle` until the bound line fires, the
/// `timeout_ns` deadline elapses, or the binding disappears.
///
/// This is the single implementation of the kernel IRQ blocking loop. It composes [`IrqTable::try_wait_step`] —
/// which performs the forgery check and the mask-before-wake-ordered
/// ready consume — with the caller-supplied [`IrqWaiter`] clock and
/// yield seam.
///
/// The deadline is computed once, from the first
/// [`IrqWaiter::now_ns`] reading, with a saturating add so a caller
/// passing `u64::MAX` does not wrap the deadline back to a tiny value
/// (fail closed). Pass `u64::MAX` for an
/// effectively unbounded wait (the loop still terminates on `Ready`
/// or `NotFound`).
#[must_use]
pub fn block_until_ready(
    table: &IrqTable,
    handle: IrqHandle,
    caller: TaskId,
    timeout_ns: u64,
    waiter: &dyn IrqWaiter,
) -> WaitOutcome {
    let deadline_ns = waiter.now_ns().saturating_add(timeout_ns);
    loop {
        let now_ns = waiter.now_ns();
        match table.try_wait_step(handle, caller, now_ns, deadline_ns) {
            WaitStep::Ready => return WaitOutcome::Ready,
            WaitStep::TimedOut => return WaitOutcome::TimedOut,
            WaitStep::NotFound => return WaitOutcome::NotFound,
            WaitStep::Quarantined => return WaitOutcome::Quarantined,
            WaitStep::Continue => {
                if let Err(abort) = waiter.yield_now(deadline_ns) {
                    return WaitOutcome::Aborted(abort);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::MaskError;
    use crate::table::IrqController;
    use core::cell::Cell;

    extern crate std;

    /// Permissive controller so `IrqTable::fire` can set the ready
    /// flag without an architecture port.
    struct OkController;
    impl IrqController for OkController {
        fn mask(&self, _line: u32) -> Result<(), MaskError> {
            Ok(())
        }
    }

    /// Deterministic waiter. The monotonic clock advances by
    /// `tick_ns` on every `now_ns` reading after the first, so a
    /// finite `timeout_ns` is guaranteed to expire. `yield_calls`
    /// records how many cooperative yields the loop performed; a
    /// closure run on the `nth` yield lets a test inject a fire (the
    /// "device raises its line while the driver is parked" case).
    struct TestWaiter<'a> {
        now: Cell<u64>,
        tick_ns: u64,
        yield_calls: Cell<u32>,
        abort_after: Option<(u32, IrqWaitAbort)>,
        on_yield: Option<(u32, &'a dyn Fn())>,
        /// The bound handed to the most recent park, so a test can assert
        /// the loop tells its park what deadline it is bounded by.
        parked_until: Cell<u64>,
    }

    impl<'a> TestWaiter<'a> {
        fn new(tick_ns: u64) -> Self {
            Self {
                now: Cell::new(0),
                tick_ns,
                yield_calls: Cell::new(0),
                abort_after: None,
                on_yield: None,
                parked_until: Cell::new(0),
            }
        }

        fn aborting(abort_after: u32, reason: IrqWaitAbort) -> Self {
            Self {
                now: Cell::new(0),
                tick_ns: 0,
                yield_calls: Cell::new(0),
                abort_after: Some((abort_after, reason)),
                on_yield: None,
                parked_until: Cell::new(0),
            }
        }

        fn firing(fire_on: u32, hook: &'a dyn Fn()) -> Self {
            Self {
                now: Cell::new(0),
                tick_ns: 0,
                yield_calls: Cell::new(0),
                abort_after: None,
                on_yield: Some((fire_on, hook)),
                parked_until: Cell::new(0),
            }
        }
    }

    impl IrqWaiter for TestWaiter<'_> {
        fn now_ns(&self) -> u64 {
            self.now.get()
        }

        fn yield_now(&self, deadline_ns: u64) -> Result<(), IrqWaitAbort> {
            let n = self.yield_calls.get() + 1;
            self.yield_calls.set(n);
            self.parked_until.set(deadline_ns);
            if let Some((fire_on, hook)) = self.on_yield {
                if n == fire_on {
                    hook();
                }
            }
            if let Some((abort_after, reason)) = self.abort_after {
                if n >= abort_after {
                    return Err(reason);
                }
            }
            self.now.set(self.now.get().saturating_add(self.tick_ns));
            Ok(())
        }
    }

    #[test]
    fn returns_ready_when_pre_fired() {
        let table = IrqTable::new(31);
        let out = table.bind(7, TaskId(1)).unwrap();
        table.fire(7, &OkController).unwrap();
        let waiter = TestWaiter::new(1);
        assert_eq!(
            block_until_ready(&table, out.handle, TaskId(1), 1_000, &waiter),
            WaitOutcome::Ready
        );
        // A pre-fired binding is consumed on the first poll, before
        // any yield.
        assert_eq!(waiter.yield_calls.get(), 0);
    }

    #[test]
    fn returns_ready_when_fire_arrives_during_a_yield() {
        let table = IrqTable::new(31);
        let out = table.bind(7, TaskId(1)).unwrap();
        let fire = || {
            table.fire(7, &OkController).unwrap();
        };
        // The device raises its line on the third parked yield.
        let waiter = TestWaiter::firing(3, &fire);
        assert_eq!(
            block_until_ready(&table, out.handle, TaskId(1), u64::MAX, &waiter),
            WaitOutcome::Ready
        );
        assert_eq!(waiter.yield_calls.get(), 3);
    }

    #[test]
    fn returns_timed_out_when_deadline_elapses() {
        let table = IrqTable::new(31);
        let out = table.bind(7, TaskId(1)).unwrap();
        // Clock advances 100 ns per yield; a 250 ns budget expires
        // after three readings.
        let waiter = TestWaiter::new(100);
        assert_eq!(
            block_until_ready(&table, out.handle, TaskId(1), 250, &waiter),
            WaitOutcome::TimedOut
        );
    }

    #[test]
    fn the_park_is_told_the_deadline_the_loop_is_bounded_by() {
        // A bounded wait is only bounded in fact if the park can be released
        // without the line firing: the loop therefore hands its own deadline
        // to every park. A park told nothing could only be woken by the line,
        // so a lost completion interrupt would strand the task forever while
        // it holds the device's lock.
        let table = IrqTable::new(31);
        let out = table.bind(7, TaskId(1)).unwrap();
        let waiter = TestWaiter::new(100);
        assert_eq!(
            block_until_ready(&table, out.handle, TaskId(1), 250, &waiter),
            WaitOutcome::TimedOut
        );
        assert_eq!(waiter.parked_until.get(), 250);

        // `u64::MAX` is the caller asking for no deadline, and is passed
        // through unchanged rather than saturating into a near-term bound.
        // The abort ends this wait after its first park (an unbounded wait
        // whose line never fires has no other terminal step, which is the
        // whole reason a *request* wait must never ask for one).
        let unbounded = TestWaiter::aborting(1, IrqWaitAbort::TaskVanished);
        assert_eq!(
            block_until_ready(&table, out.handle, TaskId(1), u64::MAX, &unbounded),
            WaitOutcome::Aborted(IrqWaitAbort::TaskVanished)
        );
        assert_eq!(unbounded.parked_until.get(), u64::MAX);
    }

    #[test]
    fn returns_not_found_on_forged_handle() {
        let table = IrqTable::new(31);
        let waiter = TestWaiter::new(1);
        assert_eq!(
            block_until_ready(
                &table,
                IrqHandle::from_raw(0xDEAD_BEEF),
                TaskId(1),
                u64::MAX,
                &waiter
            ),
            WaitOutcome::NotFound
        );
    }

    #[test]
    fn propagates_yield_abort() {
        let table = IrqTable::new(31);
        let out = table.bind(7, TaskId(1)).unwrap();
        let waiter = TestWaiter::aborting(1, IrqWaitAbort::TaskVanished);
        assert_eq!(
            block_until_ready(&table, out.handle, TaskId(1), u64::MAX, &waiter),
            WaitOutcome::Aborted(IrqWaitAbort::TaskVanished)
        );
    }

    #[test]
    fn returns_quarantined_when_the_line_was_disabled_by_the_safety_net() {
        use crate::table::MonotonicClock;
        use core::sync::atomic::{AtomicU64, Ordering};

        struct ZeroClock(AtomicU64);
        impl MonotonicClock for ZeroClock {
            fn now_ns(&self) -> u64 {
                self.0.load(Ordering::Relaxed)
            }
        }

        let table = IrqTable::new(31);
        // Freeze time so every fire lands in one window and the budget trips.
        table
            .set_clock(std::boxed::Box::leak(std::boxed::Box::new(ZeroClock(
                AtomicU64::new(0),
            ))))
            .expect("clock installs once");
        let out = table.bind(7, TaskId(1)).unwrap();
        for _ in 0..=crate::table::STORM_FIRE_BUDGET {
            let _ = table.fire(7, &OkController);
        }
        // The blocking loop surfaces the quarantine terminally, without ever
        // yielding: the driver fails closed instead of re-arming into a storm.
        let waiter = TestWaiter::new(1);
        assert_eq!(
            block_until_ready(&table, out.handle, TaskId(1), u64::MAX, &waiter),
            WaitOutcome::Quarantined
        );
        assert_eq!(waiter.yield_calls.get(), 0);
    }

    #[test]
    fn unbounded_timeout_does_not_wrap_the_deadline() {
        // A `u64::MAX` timeout must not wrap to a tiny deadline and
        // spuriously time out on the first poll.
        let table = IrqTable::new(31);
        let out = table.bind(7, TaskId(1)).unwrap();
        let fire = || {
            table.fire(7, &OkController).unwrap();
        };
        let waiter = TestWaiter::firing(1, &fire);
        assert_eq!(
            block_until_ready(&table, out.handle, TaskId(1), u64::MAX, &waiter),
            WaitOutcome::Ready
        );
    }
}
