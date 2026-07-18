//! Outcome of a single scheduler step.

use crate::task::TaskId;

/// What a [`crate::SchedulerPolicy::exit`] call did — the ownership handoff
/// a terminating caller needs to reclaim a task's resources **safely** on
/// SMP.
///
/// Reclaiming a task's address space (or any resource its user code can
/// still reach) while another CPU is *still executing that task* turns a
/// legitimate access into a wild fault. `exit` therefore never reports a
/// task as done while it is still running: it tells the caller who now owns
/// the final transition to quiescence.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ExitDisposition {
    /// The task was **not executing on any CPU** (Ready, Parked, or already
    /// between dispatches). `exit` transitioned it to
    /// [`crate::TaskState::Exited`], dropped its body, and removed it — it
    /// is fully quiescent now. The caller owns teardown and may reclaim the
    /// task's resources immediately.
    Quiesced,
    /// The task was **executing its body on some CPU** at the moment of the
    /// call. It has been marked for termination but stops executing — and
    /// becomes reclaimable — only when that dispatch returns to the
    /// scheduler. The caller MUST NOT reclaim now; the owning dispatch
    /// performs the final [`crate::TaskState::Exited`] transition, and the
    /// dispatch loop reclaims once the task is quiescent.
    Deferred,
    /// The id was known but the task was already terminal (or a prior
    /// termination request already owns its teardown). No teardown is owed
    /// to this caller — repeated termination requests reclaim exactly once.
    AlreadyExited,
}

/// Outcome of one [`crate::SchedulerPolicy::step`] call.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum StepOutcome {
    /// A task was dispatched. The contained `TaskId` ran exactly once.
    Ran(TaskId),
    /// No runnable work for this CPU after both the local queues and
    /// stealing were exhausted. The arch port should idle (HLT / WFI /
    /// `yield_now`).
    Idle,
}
