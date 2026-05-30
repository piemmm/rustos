//! Concrete per-task storage for the MLFQ policy.
//!
//! The policy-neutral lifecycle vocabulary ([`Priority`], [`TaskState`],
//! [`TaskAction`], [`TaskContext`], [`TaskId`]) is defined once in
//! `kernel/sched/api` (`AGENTS.md` §2.2 / §17.1) and re-exported by this
//! crate. This module owns only the MLFQ-specific representation of a
//! live task: the boxed body and the atomics the dispatch loop CAS-es.
//!
//! The body is a closure (`FnMut(&mut TaskContext) -> TaskAction`) so the
//! scheduler is host-testable; the real context-switch machinery lands
//! with the architecture ports. `park`, `unpark`, and `exit` are
//! *cancellation-safe*: they may be issued while the task is running on
//! another CPU and take effect at the next safe point. See
//! `docs/src/architecture/scheduler.md` for the full invariants.

use crate::loom_compat::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use crate::{CpuId, Priority, TaskAction, TaskContext, TaskId, TaskState};

use alloc::boxed::Box;
use rustos_sync::SpinLock;

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
    /// registry (`AGENTS.md` §13 — debugging must remain practical).
    #[allow(dead_code)] // read by debug / tracing builds only.
    pub id: TaskId,
    /// Last CPU the task ran on. Stealers update this on success so future
    /// schedules favour cache locality.
    pub home_cpu: AtomicU32,
    /// Current priority band, stored as `Priority as u8`.
    pub priority: AtomicU8,
    /// Lifecycle state, stored as `TaskState as u8`.
    pub state: AtomicU8,
    /// Total times the body has been invoked. Useful for fairness tests.
    pub total_runs: AtomicU64,
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
            state: AtomicU8::new(TaskState::Ready.as_u8()),
            total_runs: AtomicU64::new(0),
            yields_at_band: AtomicU64::new(0),
            last_started: AtomicU64::new(0),
            body: SpinLock::new(Some(body)),
        }
    }

    /// Atomically load the priority.
    pub(crate) fn load_priority(&self) -> Priority {
        // Bounded at construction; demote() always returns a valid band.
        // SAFETY-INVARIANT: only `from_index`-produced values are ever
        // stored, so the fallback to High here is unreachable in
        // practice. The fallback exists to satisfy AGENTS.md §2.9 (no
        // panic in production paths) without an unsafe transmute.
        let raw = self.priority.load(Ordering::Acquire) as usize;
        Priority::from_index(raw).unwrap_or(Priority::High)
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
}
