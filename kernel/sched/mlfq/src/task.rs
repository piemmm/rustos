//! Concrete per-task storage for the MLFQ policy.
//!
//! The policy-neutral lifecycle vocabulary ([`Priority`], [`TaskState`],
//! [`TaskAction`], [`TaskContext`], [`TaskId`]) is defined once in
//! `kernel/sched/api` and re-exported by this
//! crate. This module owns only the MLFQ-specific representation of a
//! live task: the boxed body and the atomics the dispatch loop CAS-es.
//!
//! The body is a closure (`FnMut(&mut TaskContext) -> TaskAction`) so the
//! scheduler is host-testable; the real context-switch machinery lands
//! with the architecture ports. `park`, `unpark`, and `exit` are
//! *cancellation-safe*: they may be issued while the task is running on
//! another CPU and take effect at the next safe point. See
//! `docs/src/architecture/scheduler.md` for the full invariants.

use crate::loom_compat::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use crate::{CpuId, Priority, SchedClass, TaskAction, TaskContext, TaskId, TaskState};

use alloc::boxed::Box;
use tairix_sync::SpinLock;

/// Concrete closure type stored inside a task. Boxed and trait-object'd
/// because tasks are owned heterogeneously by [`crate::Scheduler`].
pub(crate) type TaskBody = dyn FnMut(&mut TaskContext) -> TaskAction + Send + 'static;

/// Per-task data shared between the scheduler and any holder of the task.
///
/// Wrapped in `Arc<TaskInner>` and tracked by a global registry inside
/// [`crate::Scheduler`]. The body is locked by a `SpinLock` so that a
/// concurrent `exit()` can safely tear it down once execution has yielded.
pub(crate) struct TaskInner {
    /// Stable identity. Mirrored from the registry key so kernel-side
    /// logging / panic paths can stamp records without re-locking the
    /// registry (debugging must remain practical).
    pub id: TaskId,
    /// Last CPU the task ran on. Stealers update this on success so future
    /// schedules favour cache locality.
    pub home_cpu: AtomicU32,
    /// Current priority band, stored as `Priority as u8`.
    pub priority: AtomicU8,
    /// Scheduling class, stored as `SchedClass as u8`. A
    /// [`SchedClass::Realtime`] task is dispatched ahead of every
    /// time-shared band on its CPU and never preempted by one; the default
    /// (and every newly spawned task) is [`SchedClass::TimeShared`]. The
    /// MLFQ band/demotion bookkeeping is meaningful only for the
    /// time-shared class.
    pub sched_class: AtomicU8,
    /// Lifecycle state, stored as `TaskState as u8`.
    pub state: AtomicU8,
    /// Total times the body has been invoked. Useful for fairness tests.
    pub total_runs: AtomicU64,
    /// Cumulative ticks the body has spent running, in
    /// [`crate::SchedulerArch::ticks_now`] units. Accumulated by the
    /// dispatch loop around each body invocation; read by the
    /// `cpu_ticks_of` observation for the System Information feed.
    pub run_ticks: AtomicU64,
    /// Number of *consecutive* `Yield`s at the current priority. Resets
    /// on demotion, promotion, or park.
    pub yields_at_band: AtomicU64,
    /// Tick at which the task last started running. Used by tests for
    /// latency / starvation measurements.
    pub last_started: AtomicU64,
    /// The closure itself. `None` after [`TaskState::Exited`] so the
    /// allocation is reclaimed immediately rather than living as long as
    /// the registry entry.
    pub body: SpinLock<Option<Box<TaskBody>>>,
    /// Wake-pending token closing the park/unpark lost-wakeup race
    /// (no lost wake-ups). An [`crate::Scheduler::unpark`]
    /// that arrives while the task has not yet committed to park cannot move
    /// a non-parked task, so it sets this flag; the dispatch loop's `Park`
    /// commit consumes it and re-readies the task rather than sleeping it.
    /// Mirrors Rust's `Thread` park/unpark token semantics.
    pub wake_pending: AtomicBool,
    /// Termination request against a task that was still executing when
    /// [`crate::Scheduler::exit`] was called. Set once (first request wins);
    /// the owning dispatch observes it when its body returns and performs
    /// the final transition to [`TaskState::Exited`] itself, so a task
    /// killed while running is never reclaimed by the killer while it is
    /// still on-CPU.
    pub doomed: AtomicBool,
}

impl TaskInner {
    /// Construct a fresh task in the [`TaskState::Ready`] state.
    pub(crate) fn new(
        id: TaskId,
        home_cpu: CpuId,
        priority: Priority,
        body: Box<TaskBody>,
    ) -> Self {
        Self {
            id,
            home_cpu: AtomicU32::new(home_cpu),
            priority: AtomicU8::new(priority as u8),
            sched_class: AtomicU8::new(SchedClass::TimeShared.as_u8()),
            state: AtomicU8::new(TaskState::Ready.as_u8()),
            total_runs: AtomicU64::new(0),
            run_ticks: AtomicU64::new(0),
            yields_at_band: AtomicU64::new(0),
            last_started: AtomicU64::new(0),
            body: SpinLock::new(Some(body)),
            wake_pending: AtomicBool::new(false),
            doomed: AtomicBool::new(false),
        }
    }

    /// Record that a wake arrived before the task committed to park, so the
    /// next park is cancelled (no lost wake-ups).
    pub(crate) fn set_wake_pending(&self) {
        self.wake_pending.store(true, Ordering::Release);
    }

    /// Atomically consume the wake-pending token, returning whether one was
    /// set. Called at the dispatch-loop `Park` commit: a `true` cancels the
    /// park (the task is re-readied instead of slept).
    pub(crate) fn take_wake_pending(&self) -> bool {
        self.wake_pending.swap(false, Ordering::AcqRel)
    }

    /// Atomically load the priority.
    pub(crate) fn load_priority(&self) -> Priority {
        // Bounded at construction; demote() always returns a valid band.
        // SAFETY-INVARIANT: only `from_index`-produced values are ever
        // stored, so the fallback to High here is unreachable in
        // practice. The fallback exists to satisfy (no
        // panic in production paths) without an unsafe transmute.
        let raw = self.priority.load(Ordering::Acquire) as usize;
        Priority::from_index(raw).unwrap_or(Priority::High)
    }

    /// Atomically place the task in the `priority` band and reset its
    /// consecutive-yield count, so it starts fresh residency there.
    ///
    /// Takes effect at the task's next enqueue. Used by the
    /// anti-starvation boost and the external priority change alike; the
    /// demotion rule keeps its own interleaved counter arithmetic.
    pub(crate) fn store_priority(&self, priority: Priority) {
        self.priority.store(priority as u8, Ordering::Release);
        self.yields_at_band.store(0, Ordering::Release);
    }

    /// Atomically load the scheduling class.
    pub(crate) fn load_sched_class(&self) -> SchedClass {
        // Only `SchedClass`-produced values are ever stored; a corrupt byte
        // fails safe to the non-privileged time-shared band rather than
        // silently granting real-time priority.
        let raw = self.sched_class.load(Ordering::Acquire);
        SchedClass::from_u8(raw).unwrap_or(SchedClass::TimeShared)
    }

    /// Atomically store the scheduling class.
    pub(crate) fn store_sched_class(&self, class: SchedClass) {
        self.sched_class.store(class.as_u8(), Ordering::Release);
    }

    /// Atomically load the state.
    pub(crate) fn load_state(&self) -> TaskState {
        let raw = self.state.load(Ordering::Acquire);
        TaskState::from_u8(raw).unwrap_or(TaskState::Exited)
    }

    /// CAS the state; returns `Ok(())` on success, `Err(current)` otherwise.
    pub(crate) fn cas_state(&self, expected: TaskState, new: TaskState) -> Result<(), TaskState> {
        match self.state.compare_exchange(
            expected.as_u8(),
            new.as_u8(),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(cur) => Err(TaskState::from_u8(cur).unwrap_or(TaskState::Exited)),
        }
    }

    /// Unconditionally store the state (only for "any → Exited" sweeps).
    pub(crate) fn store_state(&self, new: TaskState) {
        self.state.store(new.as_u8(), Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cas_state_transitions() {
        let t = TaskInner::new(1, 0, Priority::Normal, Box::new(|_| TaskAction::Exit));
        assert_eq!(t.load_state(), TaskState::Ready);
        t.cas_state(TaskState::Ready, TaskState::Running)
            .expect("ready -> running");
        assert_eq!(t.load_state(), TaskState::Running);
        assert!(t.cas_state(TaskState::Ready, TaskState::Running).is_err());
    }

    #[test]
    fn store_priority_places_the_band_and_resets_yield_residency() {
        let t = TaskInner::new(1, 0, Priority::High, Box::new(|_| TaskAction::Exit));
        t.yields_at_band.store(3, Ordering::Release);
        t.store_priority(Priority::Low);
        assert_eq!(t.load_priority(), Priority::Low);
        assert_eq!(t.yields_at_band.load(Ordering::Acquire), 0);
    }
}
