//! The architecture-neutral trap callbacks every port installs.
//!
//! A port's timer interrupt, its return-to-user preempt point, and its
//! reschedule IPI each need the *same* kernel-side work done; only the
//! trap plumbing that reaches them is target-divergent. These are the
//! single definitions of that work, installed directly by each port's
//! wiring rather than re-stated per port, so a new port inherits the
//! whole body and none can drive a tick that performs only part of it.
//! Three ports each carrying their own copy is how wasm32 came to drive
//! no deadline sweep at all.
//!
//! Every callback here is pure accounting except
//! [`on_user_preempt_point`], which context-switches and is therefore
//! reached only from a trap taken in user mode. That is what makes the
//! others safe on a trap taken with the kernel mid-operation.

use tairix_arch_api::CpuId;

/// Honour `cpu`'s pending reschedule on the way back to user mode.
///
/// Installed as each port's return-to-user preempt callback and reached
/// on **any** interrupt returning to user mode. It reschedules only when
/// this CPU owes one, so an interrupt that woke nothing returns straight
/// to user mode with no gratuitous context switch; a stray invocation
/// with no resumable user task published is a no-op rather than an
/// unsound switch.
pub extern "C" fn on_user_preempt_point(cpu: CpuId) {
    let _ = crate::preempt::preempt_current(cpu);
}

/// Perform every per-tick duty on `cpu`.
///
/// Installed as each port's timer-tick callback and invoked on **every**
/// fired tick, whichever privilege level it interrupted:
///
/// * [`note_preempt_tick`](crate::note_preempt_tick) latches the tick, so
///   a quantum that expires inside a syscall is honoured at that
///   syscall's completion instead of being silently lost.
/// * [`timed_wake_sweep`](crate::timed_wake_sweep) requests the
///   blocking-wait deadline sweep, which is what makes a finite timeout
///   fire when every task is parked and no preemption is owed.
/// * [`check_stall`](crate::check_stall) samples the stall watchdog: a
///   tick still fires on a CPU looping without returning to the
///   scheduler, so this is where a soft lockup becomes observable.
///
/// All three are pure accounting and never context-switch, so a tick
/// taken in kernel mode is safe; preempting a *user* task is the separate
/// [`on_user_preempt_point`].
pub extern "C" fn on_timer_tick(cpu: CpuId) {
    crate::preempt::note_preempt_tick(cpu);
    crate::waitq::timed_wake_sweep();
    crate::watchdog::check_stall(cpu);
}

/// Latch a delivered reschedule IPI as `cpu`'s pending preemption.
///
/// Installed as each port's reschedule-IPI callback. A user task on the
/// targeted CPU then yields at its next preemption point, so cross-CPU
/// placement is honoured promptly on a busy core too. Pure accounting —
/// the context switch is [`on_user_preempt_point`]'s job.
pub extern "C" fn on_reschedule_ipi(cpu: CpuId) {
    crate::preempt::note_preempt_tick(cpu);
}

#[cfg(test)]
mod tests {
    use super::{on_reschedule_ipi, on_timer_tick, on_user_preempt_point};
    use crate::preempt::{preemption_count, take_preempt_pending};
    use tairix_arch_api::CpuId;

    /// The per-CPU slots are process-wide, so each case owns its own CPU
    /// index and never observes another test thread's latch.
    #[test]
    fn the_timer_tick_latches_the_pending_preemption() {
        const CPU: CpuId = 36;
        on_timer_tick(CPU);
        assert!(
            take_preempt_pending(CPU),
            "a fired tick must latch the CPU's pending preemption"
        );
    }

    #[test]
    fn the_reschedule_ipi_latches_the_pending_preemption() {
        const CPU: CpuId = 37;
        on_reschedule_ipi(CPU);
        assert!(
            take_preempt_pending(CPU),
            "a delivered reschedule IPI must latch the CPU's pending preemption"
        );
    }

    /// With nothing owed the preempt point must neither count a preemption
    /// nor fault on a CPU with no user task switched in.
    #[test]
    fn the_user_preempt_point_is_a_no_op_when_nothing_is_owed() {
        const CPU: CpuId = 38;
        assert_eq!(preemption_count(CPU), 0);
        on_user_preempt_point(CPU);
        assert_eq!(preemption_count(CPU), 0);
    }
}
