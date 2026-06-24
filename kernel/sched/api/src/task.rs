//! Task lifecycle vocabulary shared by every scheduler policy.
//!
//! A task is the smallest unit a scheduler dispatches. Conceptually it is
//! an in-kernel thread of control: an identity, a state machine, and a
//! body to execute on its turn. These types are policy-neutral — the
//! concrete per-task storage (the boxed body, the atomics) lives in each
//! `kernel/sched/<impl>` crate, not here.
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
//! `park`, `unpark`, and `exit` are *cancellation-safe*: an implementation
//! may be issued one while the task is running on another CPU and must
//! apply it at the next safe point. See `docs/src/architecture/scheduler.md`
//! for the full invariants.

use crate::arch::CpuId;

/// Stable identifier for a task. Reserved value `0` means "no task".
///
/// IDs are assigned monotonically by [`crate::SchedulerPolicy::spawn`].
/// They are never recycled within the lifetime of a single scheduler,
/// which keeps stale references harmless: a re-used ID could otherwise
/// let a parked caller wake the wrong task. With `u64` width the counter
/// cannot realistically wrap in any kernel uptime.
pub type TaskId = u64;

/// Priority band a task occupies.
///
/// Three bands are sufficient for an MLFQ-style policy (see
/// `docs/src/architecture/scheduler.md`). Adding more bands is an explicit
/// interface change (no interface creep): a run-queue
/// type sizes itself with `Priority::COUNT` worth of per-CPU deques.
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
    /// Used by an MLFQ demotion rule: a task that consumes a full
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
/// An implementation stores this (typically in an `AtomicU8`) inside each
/// task. Allowed transitions:
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
/// Returned from the closure passed to [`crate::SchedulerPolicy::spawn`].
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
}
