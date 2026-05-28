//! Task representation and lifecycle types.
//!
//! A task is the smallest unit the scheduler dispatches. Conceptually it
//! is an in-kernel thread of control: an identity, a state machine, and a
//! body to execute on its turn. The body is a closure
//! (`FnMut(&mut TaskContext) -> TaskAction`) so this Stage-2.3 deliverable
//! is host-testable; the real context-switch machinery lands with the
//! architecture ports in Stage 3 of `PLAN.md`.
//!
//! ## State machine
//!
//! ```text
//!         spawn                     unpark
//!           │                          │
//!           ▼                          │
//!         Ready ◀──── Yield ────── Running ──── Park ───▶ Parked
//!                                     │                     │
//!                                     └──── Exit ───▶ Exited
//! ```
//!
//! Transitions are encoded as compare-and-swap on an atomic state
//! discriminant inside each task.
//! `park`, `unpark`, and `exit` are *cancellation-safe*: they may be
//! issued while the task is running on another CPU and will take effect
//! at the next safe point. See `docs/src/architecture/scheduler.md` for
//! the full invariants.

use crate::arch::CpuId;
use crate::loom_compat::{AtomicU32, AtomicU64, AtomicU8, Ordering};

use alloc::boxed::Box;
use rustos_kernel_sync::SpinLock;

/// Stable identifier for a task. Reserved value `0` means "no task".
///
/// IDs are assigned monotonically by [`crate::Scheduler::spawn`]. They are
/// never recycled within the lifetime of a single scheduler, which keeps
/// stale references harmless: a re-used ID could otherwise let a parked
/// caller wake the wrong task. With `u64` width the counter cannot
/// realistically wrap in any kernel uptime.
pub type TaskId = u64;

/// Priority band a task occupies.
///
/// Three bands are sufficient for an MLFQ-style policy (see
/// `docs/src/architecture/scheduler.md`). Adding more bands is an explicit
/// interface change (`AGENTS.md` §2.4 — no interface creep): the run-queue
/// type carries `Priority::COUNT` worth of per-CPU deques.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Priority {
    /// Highest priority: interactive / short-running tasks.
    High = 0,
    /// Default priority: most kernel work.
    Normal = 1,
    /// Background work; ready to be preempted by either band above.
    Low = 2,
}

impl Priority {
    /// Number of bands. Run-queues sized at construction with this constant.
    pub const COUNT: usize = 3;

    /// Returns the band index (`0..COUNT`).
    #[must_use]
    pub const fn as_index(self) -> usize {
        self as u8 as usize
    }

    /// Returns the priority for a band index, or `None` for out-of-range.
    #[must_use]
    pub const fn from_index(i: usize) -> Option<Self> {
        match i {
            0 => Some(Self::High),
            1 => Some(Self::Normal),
            2 => Some(Self::Low),
            _ => None,
        }
    }

    /// One step less urgent (or `self` if already [`Priority::Low`]).
    ///
    /// Used by the MLFQ demotion rule: a task that consumes a full
    /// quantum without yielding voluntarily is demoted on the next
    /// re-enqueue.
    #[must_use]
    pub const fn demote(self) -> Self {
        match self {
            Self::High => Self::Normal,
            Self::Normal | Self::Low => Self::Low,
        }
    }
}

/// Task lifecycle state.
///
/// Stored in an `AtomicU8` inside each task. Allowed transitions:
///
/// | from      | to         | trigger                         |
/// | --------- | ---------- | ------------------------------- |
/// | `Ready`   | `Running`  | scheduler picks the task        |
/// | `Running` | `Ready`    | body returns [`TaskAction::Yield`] |
/// | `Running` | `Parked`   | body returns [`TaskAction::Park`] or external `park` |
/// | `Running` | `Exited`   | body returns [`TaskAction::Exit`] or external `exit` |
/// | `Ready`   | `Parked`   | external `park` while queued    |
/// | `Parked`  | `Ready`    | external `unpark`               |
/// | any       | `Exited`   | external `exit`                 |
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum TaskState {
    /// In a run queue or about to be enqueued.
    Ready = 0,
    /// Currently executing on some CPU.
    Running = 1,
    /// Not runnable until a matching `unpark`.
    Parked = 2,
    /// Terminal state. The task body has been dropped.
    Exited = 3,
}

impl TaskState {
    /// Returns the raw discriminant as stored in an atomic.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Inverse of [`Self::as_u8`]; returns `None` for unknown encodings.
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Ready),
            1 => Some(Self::Running),
            2 => Some(Self::Parked),
            3 => Some(Self::Exited),
            _ => None,
        }
    }
}

/// What the scheduler should do with a task whose body has just returned.
///
/// Returned from the closure passed to [`crate::Scheduler::spawn`].
/// Combined with externally-issued `park`/`unpark`/`exit`, this gives the
/// scheduler a full picture of the task's intent.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TaskAction {
    /// Re-enqueue at the current priority (subject to MLFQ demotion).
    Yield,
    /// Transition to [`TaskState::Parked`]; do not re-enqueue.
    Park,
    /// Terminal: transition to [`TaskState::Exited`] and drop the body.
    Exit,
}

/// Argument passed to a task body on each scheduling step.
///
/// Tests and userland alike read the current CPU and tick to make
/// reproducible decisions (e.g. cooperative time-slice accounting).
#[derive(Copy, Clone, Debug)]
pub struct TaskContext {
    /// The CPU dispatching this task.
    pub cpu: CpuId,
    /// The arch-provided tick at the start of this step.
    pub tick: u64,
    /// The task's identity.
    pub task_id: TaskId,
}

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
    fn priority_round_trip() {
        for i in 0..Priority::COUNT {
            let p = Priority::from_index(i).expect("valid band");
            assert_eq!(p.as_index(), i);
        }
        assert!(Priority::from_index(Priority::COUNT).is_none());
    }

    #[test]
    fn priority_demote_saturates() {
        assert_eq!(Priority::High.demote(), Priority::Normal);
        assert_eq!(Priority::Normal.demote(), Priority::Low);
        assert_eq!(Priority::Low.demote(), Priority::Low);
    }

    #[test]
    fn task_state_round_trip() {
        for s in [
            TaskState::Ready,
            TaskState::Running,
            TaskState::Parked,
            TaskState::Exited,
        ] {
            assert_eq!(TaskState::from_u8(s.as_u8()), Some(s));
        }
        assert_eq!(TaskState::from_u8(99), None);
    }

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
