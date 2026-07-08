//! Per-CPU pending-preemption latch: the bridge between a timer tick the
//! kernel could not act on immediately and the next point it safely can.
//!
//! The kernel is non-preemptible: a one-shot preemption tick taken while
//! the CPU is in kernel mode (mid-syscall, or in a kernel kthread) must
//! never context-switch away from a half-completed kernel critical
//! section, so the ports' IRQ paths preempt only a tick taken from user
//! mode. Before this latch existed, a quantum that expired *inside* a
//! syscall was silently lost — the fired one-shot was disarmed, nothing
//! recorded that the running task's turn was over, and the task returned
//! to user mode with no timer armed, free to run unpreempted until its
//! next voluntary yield. A task that regularly sat in syscalls (a shell
//! tool walking the filesystem, a process writing the console) could
//! therefore starve every competitor: cooperative scheduling in
//! preemptive clothing.
//!
//! The latch closes that hole with three sites, all on this CPU:
//!
//! * **Set** — the per-tick callback every port's timer IRQ path invokes
//!   on every fired one-shot calls [`note_preempt_tick`]. Latching is
//!   pure accounting (one atomic store), so it is safe on a tick taken
//!   in kernel mode.
//! * **Consume** — when a syscall completes, the production dispatch
//!   hook calls [`take_preempt_pending`]; a latched tick converts the
//!   syscall's ordinary return into the same suspend-back-to-scheduler
//!   path a `yield` syscall takes, so the expired turn is honoured at
//!   the first safe boundary. The running task can now overrun its
//!   quantum by at most the remainder of one bounded syscall, never
//!   indefinitely.
//! * **Clear** — the dispatcher calls `clear_preempt_pending`
//!   immediately before switching a task in: the scheduler has just made
//!   a fresh decision (and re-armed the one-shot for a contended CPU),
//!   which supersedes any tick latched before the switch. This is what
//!   keeps a user-mode tick — which already preempts immediately through
//!   the ports' user-mode preempt point — from also triggering a
//!   spurious yield on the resumed task's next syscall.
//!
//! A tick is latched whether the one-shot fired for a preemption quantum
//! or for a blocking-wait wakeup: both mean the scheduler may have newly
//! runnable work, and the user-mode path already yields on either (the
//! ports preempt on every user-taken timer tick). The next dispatch
//! re-evaluates and re-arms, so an unnecessary yield costs one bounded
//! scheduler round-trip and is never wrong; a *missed* expiry is the
//! starvation defect this module exists to prevent.

use core::sync::atomic::{AtomicBool, Ordering};

use rustos_kernel_sched_api::CpuId;

use crate::kthread::KTHREAD_MAX_CPUS;

/// Slot `cpu` is `true` while a timer tick taken on that CPU awaits its
/// preemption point.
///
/// Sized like the sibling per-CPU kthread tables ([`KTHREAD_MAX_CPUS`]).
/// Plain atomics, no lock: the setter runs in timer-IRQ context on the
/// same CPU the consumer runs on, where taking any lock could deadlock
/// against the interrupted holder.
static PREEMPT_PENDING: [AtomicBool; KTHREAD_MAX_CPUS] =
    [const { AtomicBool::new(false) }; KTHREAD_MAX_CPUS];

/// Record that `cpu`'s one-shot timer fired, so the task running there
/// owes the scheduler a reschedule at its next preemption point.
///
/// Called from the ports' per-tick timer callbacks on **every** fired
/// one-shot, regardless of the interrupted privilege level. Pure
/// accounting — it never context-switches — so it is safe from interrupt
/// context with the kernel mid-operation. An out-of-range `cpu` is
/// dropped rather than indexing out of bounds (fail closed).
pub fn note_preempt_tick(cpu: CpuId) {
    if let Some(slot) = PREEMPT_PENDING.get(cpu as usize) {
        slot.store(true, Ordering::Release);
    }
}

/// Consume `cpu`'s pending-preemption latch: `true` exactly once per
/// latched tick.
///
/// Called by the syscall dispatch hook after a syscall completes; a
/// `true` return obliges the caller to suspend the current task back to
/// the scheduler (the involuntary analogue of a `yield` syscall) before
/// returning to user mode. An out-of-range `cpu` reports `false`
/// (fail closed: no phantom reschedule for a CPU with no slot).
#[must_use]
pub fn take_preempt_pending(cpu: CpuId) -> bool {
    PREEMPT_PENDING
        .get(cpu as usize)
        .is_some_and(|slot| slot.swap(false, Ordering::AcqRel))
}

/// Discard any tick latched on `cpu` before the scheduler's current
/// dispatch decision.
///
/// Called by the dispatcher immediately before switching a task in: the
/// decision (and the freshly re-armed one-shot) supersedes a tick that
/// fired before the switch, so the incoming task starts its quantum
/// without inheriting a stale reschedule obligation. An out-of-range
/// `cpu` is a no-op.
pub(crate) fn clear_preempt_pending(cpu: CpuId) {
    if let Some(slot) = PREEMPT_PENDING.get(cpu as usize) {
        slot.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests share the process-wide per-CPU slots; each test uses its own
    /// CPU index so parallel test threads never observe each other.
    #[test]
    fn a_latched_tick_is_consumed_exactly_once() {
        const CPU: CpuId = 40;
        assert!(!take_preempt_pending(CPU));
        note_preempt_tick(CPU);
        assert!(take_preempt_pending(CPU));
        // Consumed: a second take sees nothing.
        assert!(!take_preempt_pending(CPU));
    }

    #[test]
    fn repeated_ticks_before_the_preemption_point_coalesce() {
        const CPU: CpuId = 41;
        note_preempt_tick(CPU);
        note_preempt_tick(CPU);
        assert!(take_preempt_pending(CPU));
        assert!(!take_preempt_pending(CPU));
    }

    #[test]
    fn clearing_supersedes_a_latched_tick() {
        const CPU: CpuId = 42;
        note_preempt_tick(CPU);
        clear_preempt_pending(CPU);
        assert!(!take_preempt_pending(CPU));
    }

    #[test]
    fn each_cpu_has_its_own_latch() {
        const CPU_A: CpuId = 43;
        const CPU_B: CpuId = 44;
        note_preempt_tick(CPU_A);
        assert!(!take_preempt_pending(CPU_B));
        assert!(take_preempt_pending(CPU_A));
    }

    #[test]
    fn an_out_of_range_cpu_fails_closed() {
        let beyond = u32::try_from(KTHREAD_MAX_CPUS).unwrap_or(u32::MAX);
        note_preempt_tick(beyond);
        assert!(!take_preempt_pending(beyond));
        clear_preempt_pending(beyond);
        note_preempt_tick(u32::MAX);
        assert!(!take_preempt_pending(u32::MAX));
    }
}
