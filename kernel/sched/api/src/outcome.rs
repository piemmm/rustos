//! Outcome of a single scheduler step.

use crate::task::TaskId;

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
