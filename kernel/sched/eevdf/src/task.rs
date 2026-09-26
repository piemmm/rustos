//! Concrete per-task storage for the EEVDF policy.
//!
//! The policy-neutral lifecycle vocabulary ([`Priority`], [`TaskState`],
//! [`TaskAction`], [`TaskContext`], [`TaskId`]) is defined once in
//! `kernel/sched/api` and re-exported by this crate, as are the band weights
//! and the ledger recording where a task's weight is counted. This module owns
//! only the EEVDF-specific representation of a live task: the boxed body, the
//! lifecycle atomics, and the virtual eligible time and deadline the dispatch
//! loop reads and writes.
//!
//! The body is a closure (`FnMut(&mut TaskContext) -> TaskAction`) so the
//! scheduler is host-testable; the real context-switch machinery lands
//! with the architecture ports. `park`, `unpark`, and `exit` are
//! *cancellation-safe*: they may be issued while the task is running on
//! another CPU and take effect at the next safe point.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

use alloc::boxed::Box;
use tairix_sync::SpinLock;

use crate::{CpuId, Priority, SchedClass, TaskAction, TaskContext, TaskId, TaskState};
use tairix_kernel_sched_api::share::{weight_of, WeightLedger};
use tairix_kernel_sched_api::ParkableTask;

/// Concrete closure type stored inside a task. Boxed and trait-object'd
/// because tasks are owned heterogeneously by [`crate::Scheduler`].
pub(crate) type TaskBody = dyn FnMut(&mut TaskContext) -> TaskAction + Send + 'static;

/// Per-task data shared between the scheduler and any holder of the task.
///
/// Wrapped in `Arc<TaskInner>` and tracked by a registry inside
/// [`crate::Scheduler`]. The body is locked by a `SpinLock` so that a
/// concurrent `exit()` can safely tear it down once execution has
/// yielded.
pub(crate) struct TaskInner {
    /// Stable identity. Mirrored from the registry key so kernel-side
    /// logging / panic paths can stamp records without re-locking the
    /// registry (debugging must remain practical).
    pub id: TaskId,
    /// CPU whose virtual-time run queue currently owns this task.
    /// Stealers update this on success so future schedules and re-queues
    /// land on the CPU that last ran the task.
    pub home_cpu: AtomicU32,
    /// Current priority band, stored as `Priority as u8`. Determines the
    /// task's [`weight_of`] weight: EEVDF charges virtual time inversely to
    /// it, so a heavier task is dispatched proportionally more often.
    pub priority: AtomicU8,
    /// Scheduling class, stored as `SchedClass as u8`. A
    /// [`SchedClass::Realtime`] task is dispatched ahead of every
    /// time-shared task on its CPU and never preempted by one; the default
    /// (and every newly spawned task) is [`SchedClass::TimeShared`]. The
    /// virtual-time bookkeeping below is meaningful only for the
    /// time-shared band.
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
    /// Virtual eligible time `ve` (fixed point). The task is *eligible*
    /// to run on a CPU only once that CPU's virtual time has reached
    /// `ve`; this is the "eligible" half of EEVDF.
    pub virtual_eligible: AtomicU64,
    /// Virtual deadline `vd` (fixed point) = `ve + request / weight`, where a
    /// request is one quantum of service. It moves on only once the service
    /// charged to `ve` has fulfilled the request. Among eligible tasks the
    /// scheduler dispatches the earliest `vd`; this is the "earliest virtual
    /// deadline first" half of EEVDF.
    pub virtual_deadline: AtomicU64,
    /// Tick at which the task last started running. Used by tests for
    /// latency / starvation measurements.
    pub last_started: AtomicU64,
    /// The closure itself. `None` after [`TaskState::Exited`] so the
    /// allocation is reclaimed immediately rather than living as long as
    /// the registry entry.
    pub body: SpinLock<Option<Box<TaskBody>>>,
    /// Wake-pending token closing the park/unpark lost-wakeup race
    /// (no lost wake-ups). An [`crate::Scheduler::unpark`]
    /// that arrives while the task is still [`TaskState::Running`] /
    /// [`TaskState::Ready`] (it has not yet committed to park) cannot move a
    /// non-parked task, so it instead sets this flag; the dispatch loop's
    /// `Park` commit consumes it and re-readies the task rather than sleeping
    /// it, so a wake delivered in the window between "decide to park" and
    /// "actually parked" is never dropped. Mirrors Rust's `Thread`
    /// park/unpark token semantics.
    pub wake_pending: AtomicBool,
    /// Termination request against a task that was still executing when
    /// [`crate::Scheduler::exit`] was called. Set once (first request wins);
    /// the owning dispatch observes it when its body returns and performs
    /// the final transition to [`TaskState::Exited`] itself, so a task
    /// killed while running is never reclaimed by the killer while it is
    /// still on-CPU.
    pub doomed: AtomicBool,
    /// Where this task's weight is counted: every change to a CPU's competing
    /// weight on its behalf goes through here.
    pub ledger: WeightLedger,
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
            virtual_eligible: AtomicU64::new(0),
            virtual_deadline: AtomicU64::new(0),
            last_started: AtomicU64::new(0),
            body: SpinLock::new(Some(body)),
            wake_pending: AtomicBool::new(false),
            doomed: AtomicBool::new(false),
            ledger: WeightLedger::new(),
        }
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

    /// Atomically load the priority.
    pub(crate) fn load_priority(&self) -> Priority {
        // SAFETY-INVARIANT: only `from_index`-produced values are ever
        // stored, so the fallback to High here is unreachable in
        // practice. The fallback exists to satisfy (no
        // panic in production paths) without an unsafe transmute.
        let raw = self.priority.load(Ordering::Acquire) as usize;
        Priority::from_index(raw).unwrap_or(Priority::High)
    }

    /// The task's EEVDF weight, derived from its current priority.
    pub(crate) fn weight(&self) -> u64 {
        weight_of(self.load_priority())
    }

    /// Atomically store the priority.
    ///
    /// Takes effect at the task's next enqueue: every weight read
    /// re-derives from this field, so no queued entry needs surgery.
    pub(crate) fn store_priority(&self, priority: Priority) {
        self.priority.store(priority as u8, Ordering::Release);
    }

    /// Unconditionally store the state.
    pub(crate) fn store_state(&self, new: TaskState) {
        self.state.store(new.as_u8(), Ordering::Release);
    }

    /// Store the virtual eligible / deadline pair the dispatcher computed.
    pub(crate) fn set_virtual(&self, eligible: u64, deadline: u64) {
        self.virtual_eligible.store(eligible, Ordering::Release);
        self.virtual_deadline.store(deadline, Ordering::Release);
    }

    /// Load the virtual deadline (fixed point).
    pub(crate) fn deadline(&self) -> u64 {
        self.virtual_deadline.load(Ordering::Acquire)
    }

    /// Load the virtual eligible time (fixed point).
    pub(crate) fn eligible(&self) -> u64 {
        self.virtual_eligible.load(Ordering::Acquire)
    }
}

impl ParkableTask for TaskInner {
    fn load_state(&self) -> TaskState {
        // Only `TaskState`-produced bytes are ever stored; a corrupt one
        // reads as terminal rather than panicking or reviving a task.
        let raw = self.state.load(Ordering::Acquire);
        TaskState::from_u8(raw).unwrap_or(TaskState::Exited)
    }

    fn cas_state(&self, expected: TaskState, new: TaskState) -> Result<(), TaskState> {
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

    fn set_wake_pending(&self) {
        self.wake_pending.store(true, Ordering::Release);
    }

    fn take_wake_pending(&self) -> bool {
        self.wake_pending.swap(false, Ordering::AcqRel)
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
    fn weight_tracks_priority_field() {
        let t = TaskInner::new(2, 0, Priority::Low, Box::new(|_| TaskAction::Exit));
        assert_eq!(t.weight(), weight_of(Priority::Low));
        t.store_priority(Priority::High);
        assert_eq!(t.load_priority(), Priority::High);
        assert_eq!(t.weight(), weight_of(Priority::High));
    }
}
