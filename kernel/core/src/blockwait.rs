//! [`IrqParkWaiter`] — the parking [`IrqWaiter`] an in-kernel block-device
//! completion wait drives.
//!
//! A block driver hosted in the kernel (the boot virtio-blk and EMMC2
//! devices behind `SharedBlock`) must give the CPU up while a device
//! completion is outstanding. Its waits run in whatever task context
//! reached the device — a user task inside an `fs_*` syscall, or an
//! in-kernel service kthread — and the wait may last milliseconds on real
//! hardware, so the discipline is the same one the `irq_wait` syscall
//! handler uses: **park the calling task off the run queue** and let the
//! device-IRQ dispatch path's [`irq_wake`](crate::waitq::irq_wake) unpark
//! it. Parking the *task* keeps the dispatch loop running everything else —
//! other tasks, the buffered console transmit top-up, the timed sweeps —
//! for the whole duration of the device wait.
//!
//! An earlier revision halted the *CPU* instead (a masked re-check + `wfi`
//! park inside the calling task). That starves the dispatch loop for the
//! whole device wait: on Pi 4 metal a cold-cache directory walk (`ls -lsR`)
//! stalled console output for every SD-card read and could strand the
//! system outright once the tickless one-shot was unarmed, because nothing
//! else was left to wake the halted CPU. The `wfi` shape survives here only
//! as the *fallback* for a context that cannot park (below).
//!
//! # Lost-wake interlock
//!
//! [`IrqParkWaiter::yield_now`] registers the current task on
//! [`IRQ_WAITQ`] **before** re-checking the
//! bound line's ready flag, and parks only if the flag is still clear — the same
//! register→re-test→park discipline `SleepLock` and the console read use.
//! A fire landing between the loop's consuming poll and the park is
//! therefore never lost: the flag re-check catches an early fire, and a
//! fire after the re-check finds the task registered, so the dispatcher's
//! wake (and the scheduler's wake-pending token) converts a racing park
//! into a re-ready.
//!
//! # Fallback park
//!
//! `reschedule_current` can suspend any dispatched kthread — a user task
//! inside its syscall trap and an in-kernel service kthread body alike
//! (both publish a resume handle). The only contexts it cannot suspend are
//! the boot flow before the dispatch loop runs its first task, and a host
//! test with no live dispatch loop; for those the waiter falls back to a
//! port-supplied, bounded CPU park (on aarch64 the race-free mask →
//! ready-re-check → `wfi` → unmask sequence). That boot context runs while
//! everything else is parked waiting on it, so briefly halting the CPU
//! there starves nothing; every steady-state wait takes the task-park path.

use core::sync::atomic::{AtomicU64, Ordering};

use rustos_abi::IrqHandle;
use rustos_kernel_irq::{
    block_until_ready, IrqController, IrqTable, IrqWaitAbort, IrqWaiter, WaitOutcome,
};
use rustos_kernel_sec::TaskId;

use crate::dispatch_slot::RescheduleAction;
use crate::kthread::reschedule_current;
use crate::waitq::{nearest_timed_deadline, wait_arch, IRQ_WAITQ, NO_DEADLINE};

/// A bounded CPU-park a port supplies for contexts that cannot be
/// scheduler-parked (see the module docs). It must return once the bound
/// line has fired or promptly on any other interrupt — never spin
/// unboundedly — and must tolerate spurious returns (the caller re-polls).
pub type FallbackPark = fn(&IrqTable, IrqHandle);

/// Parking [`IrqWaiter`] for one device's bound interrupt line.
///
/// One instance is built per brought-up device and lives with it for the
/// kernel's lifetime. It serves two consumers:
///
/// * [`KernelVirtioHost::notify_wait`] drives it directly through the
///   shared [`block_until_ready`] loop (unbounded wait; the used-ring
///   re-scan tolerates spurious wake-ups), and
/// * a completion seam wraps [`Self::park_wait`] for a **bounded** wait
///   that fails closed when the controller stays silent (no completion
///   *and* no error interrupt) past its budget.
///
/// [`KernelVirtioHost::notify_wait`]: ../../rustos_kernel_virtio/struct.KernelVirtioHost.html#method.notify_wait
pub struct IrqParkWaiter {
    /// The published IRQ table the device's line is bound in.
    table: &'static IrqTable,
    /// The kernel-held handle minted when the line was bound.
    handle: IrqHandle,
    /// The controller line, re-armed before every park (the dispatch
    /// path's `fire` masks it on each completion — mask-before-wake).
    line: u32,
    /// The controller the line is re-armed through.
    controller: &'static (dyn IrqController + Sync),
    /// Absolute deadline of the wait in flight ([`NO_DEADLINE`] outside
    /// [`Self::park_wait`]), registered with the wait queue so the timed
    /// sweep can wake a bounded wait whose line never fires. Device
    /// operations are serialised by the owner's `SleepLock`, so at most
    /// one wait is in flight per waiter.
    deadline_ns: AtomicU64,
    /// The port's bounded CPU-park for non-parkable contexts.
    fallback_park: FallbackPark,
}

impl IrqParkWaiter {
    /// Build the waiter for a bound device line.
    ///
    /// `table`/`handle` name the binding, `line` is the controller line to
    /// re-arm before each park, and `fallback_park` is the port's bounded
    /// CPU-park for contexts that cannot be scheduler-parked.
    #[must_use]
    pub fn new(
        table: &'static IrqTable,
        handle: IrqHandle,
        line: u32,
        controller: &'static (dyn IrqController + Sync),
        fallback_park: FallbackPark,
    ) -> Self {
        Self {
            table,
            handle,
            line,
            controller,
            deadline_ns: AtomicU64::new(NO_DEADLINE),
            fallback_park,
        }
    }

    /// Block the caller until the bound line fires or `timeout_ns`
    /// elapses, consuming the ready flag on a fire.
    ///
    /// The deadline is armed for the whole wait so the timed sweep can
    /// release a park whose line never fires; a silent controller
    /// therefore surfaces as [`WaitOutcome::TimedOut`] instead of a
    /// parked-forever task holding its mount's lock. `NotFound` (a
    /// released binding) and `Aborted` are fail-closed outcomes the
    /// caller maps to its own error surface.
    pub fn park_wait(&self, owner: TaskId, timeout_ns: u64) -> WaitOutcome {
        let deadline = self.now_ns().saturating_add(timeout_ns);
        self.deadline_ns.store(deadline, Ordering::SeqCst);
        let outcome = block_until_ready(self.table, self.handle, owner, timeout_ns, self);
        self.deadline_ns.store(NO_DEADLINE, Ordering::SeqCst);
        // Re-point the one-shot at the nearest deadline any remaining
        // timed waiter needs (or clear it) so a finished wait leaves no
        // stale arming behind.
        if let Some(hook) = wait_arch() {
            hook.set_wakeup(nearest_timed_deadline());
        }
        outcome
    }
}

impl IrqWaiter for IrqParkWaiter {
    fn now_ns(&self) -> u64 {
        // Before the scheduler hook exists no wait can be timed; the
        // constant clock makes a bounded wait behave as unbounded, and the
        // fallback park below still gives the CPU up between polls.
        wait_arch().map_or(0, crate::waitq::WaitQueueArch::now_ns)
    }

    fn yield_now(&self) -> Result<(), IrqWaitAbort> {
        // Re-arm the line before any wait: the dispatch path's `fire`
        // masked it on the previous completion (mask-before-wake). A
        // refusal is harmless — the wait is then bounded by its deadline.
        let _ = self.controller.rearm(self.line);
        let Some(hook) = wait_arch() else {
            (self.fallback_park)(self.table, self.handle);
            return Ok(());
        };
        let Some(cpu) = hook.current_cpu() else {
            (self.fallback_park)(self.table, self.handle);
            return Ok(());
        };
        let Some(task) = hook.current_task(cpu) else {
            (self.fallback_park)(self.table, self.handle);
            return Ok(());
        };
        let deadline = self.deadline_ns.load(Ordering::SeqCst);
        // Register before the re-test so a fire in the poll→park window is
        // never lost (see the module docs), then re-test: if the line
        // already fired, skip the park and let the loop's next poll
        // consume the flag.
        IRQ_WAITQ.register(task, deadline);
        if self.table.ready_for(self.handle) {
            IRQ_WAITQ.deregister(task);
            return Ok(());
        }
        // Arm the one-shot at the nearest pending deadline across every
        // timed queue so a bounded wait fires even on an otherwise-idle
        // CPU, then park off the run queue until `irq_wake` or the sweep.
        hook.set_wakeup(nearest_timed_deadline());
        if !reschedule_current(cpu, RescheduleAction::Park) {
            // Not a parkable context (the boot flow before the dispatch
            // loop runs its first task, or a host test with no live
            // dispatch loop): take the port's bounded CPU park instead of
            // parking into the void.
            IRQ_WAITQ.deregister(task);
            (self.fallback_park)(self.table, self.handle);
            return Ok(());
        }
        IRQ_WAITQ.deregister(task);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use core::sync::atomic::{AtomicU64, Ordering};

    use rustos_kernel_irq::MaskError;
    use rustos_sync::once::Once;

    use crate::waitq::{install_wait_arch, WaitQueueArch};

    /// Shared test clock/arch. Installed once for the whole test binary
    /// (the hook is set-once); every test advances the one clock through
    /// its fallback and uses relative timeouts, so tests stay independent
    /// of ordering.
    struct TestArch {
        now: AtomicU64,
    }

    impl WaitQueueArch for TestArch {
        fn unpark(&self, _id: rustos_kernel_sched_api::TaskId) {}
        fn now_ns(&self) -> u64 {
            self.now.load(Ordering::SeqCst)
        }
        fn set_wakeup(&self, _deadline_ns: Option<u64>) {}
        fn current_cpu(&self) -> Option<rustos_kernel_sched_api::CpuId> {
            Some(0)
        }
        fn current_task(
            &self,
            _cpu: rustos_kernel_sched_api::CpuId,
        ) -> Option<rustos_kernel_sched_api::TaskId> {
            Some(4242)
        }
    }

    static TEST_ARCH: TestArch = TestArch {
        now: AtomicU64::new(0),
    };
    static INSTALL: Once<()> = Once::new();

    fn arch() -> &'static TestArch {
        INSTALL
            .call_once(|| {
                install_wait_arch(&TEST_ARCH).expect("first install");
                Ok::<(), core::convert::Infallible>(())
            })
            .expect("install is infallible");
        &TEST_ARCH
    }

    /// Permissive controller so `IrqTable::fire` can mask and the waiter
    /// can re-arm without an architecture port.
    struct OkController;
    impl IrqController for OkController {
        fn mask(&self, _line: u32) -> Result<(), MaskError> {
            Ok(())
        }
    }
    static CONTROLLER: OkController = OkController;

    /// Fallback that advances the shared clock, so a bounded wait with a
    /// never-firing line reaches its deadline instead of looping forever.
    /// (Tests run in parallel over one global clock; it is monotonic, and
    /// every deadline is relative, so cross-test ticks only shorten waits.)
    fn ticking_fallback(_table: &IrqTable, _handle: IrqHandle) {
        arch().now.fetch_add(100, Ordering::SeqCst);
    }

    const LINE: u32 = 9;
    const OWNER: TaskId = TaskId(31);

    fn bound_waiter() -> (&'static IrqTable, IrqHandle, IrqParkWaiter) {
        let table: &'static IrqTable =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(IrqTable::new(31)));
        let out = table.bind(LINE, OWNER).expect("binds");
        let waiter = IrqParkWaiter::new(table, out.handle, LINE, &CONTROLLER, ticking_fallback);
        (table, out.handle, waiter)
    }

    #[test]
    fn a_pre_fired_line_is_consumed_without_any_park() {
        // A fallback that panics proves the pre-fired wait never parks.
        fn no_park(_table: &IrqTable, _handle: IrqHandle) {
            panic!("a pre-fired wait must not park at all");
        }
        let _ = arch();
        let (table, handle, _) = bound_waiter();
        let waiter = IrqParkWaiter::new(table, handle, LINE, &CONTROLLER, no_park);
        table.fire(LINE, &OkController).expect("fires");
        assert_eq!(waiter.park_wait(OWNER, 1_000_000), WaitOutcome::Ready);
        // The fire was consumed: the flag is clear for the next wait.
        assert!(!table.ready_for(handle), "ready flag must be consumed");
    }

    #[test]
    fn a_silent_line_times_out_instead_of_waiting_forever() {
        let _ = arch();
        let (_table, _handle, waiter) = bound_waiter();
        // 250 ns budget, 100 ns per fallback tick: the deadline is reached
        // after a bounded number of parks — the fail-closed outcome a dead
        // controller must produce.
        assert_eq!(waiter.park_wait(OWNER, 250), WaitOutcome::TimedOut);
    }

    #[test]
    fn a_fire_during_the_wait_wakes_and_consumes() {
        // The consuming re-poll runs after each fallback park; fire on the
        // first park through a fallback that raises the line itself.
        fn firing_fallback(table: &IrqTable, _handle: IrqHandle) {
            table.fire(LINE, &OkController).expect("fires");
        }
        let _ = arch();
        let (table, handle, _waiter) = bound_waiter();
        let waiter = IrqParkWaiter::new(table, handle, LINE, &CONTROLLER, firing_fallback);
        assert_eq!(waiter.park_wait(OWNER, u64::MAX), WaitOutcome::Ready);
        assert!(!table.ready_for(handle), "ready flag must be consumed");
    }

    #[test]
    fn a_released_binding_fails_closed_as_not_found() {
        let _ = arch();
        let (table, _handle, waiter) = bound_waiter();
        table.release_for(OWNER);
        assert_eq!(waiter.park_wait(OWNER, 1_000), WaitOutcome::NotFound);
    }

    #[test]
    fn the_deadline_is_cleared_after_a_wait() {
        let _ = arch();
        let (_table, _handle, waiter) = bound_waiter();
        let _ = waiter.park_wait(OWNER, 250);
        assert_eq!(
            waiter.deadline_ns.load(Ordering::SeqCst),
            NO_DEADLINE,
            "a finished wait must disarm its registered deadline"
        );
    }
}
