//! Error types for the scheduler contract.
//!
//! All scheduler entry points return a typed `Result` — `panic!` / `unwrap`
//! are forbidden in production paths. Each variant
//! describes a single, recoverable failure mode; callers are expected to
//! match exhaustively.

use core::fmt;

/// Every fallible scheduler operation returns this error.
///
/// The variants are deliberately coarse: scheduler entry points are called
/// from interrupt-safe paths and should not branch on detailed sub-codes.
/// Refine only when a new caller has a concrete reason to distinguish two
/// failure modes (no interface creep).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SchedError {
    /// The target CPU's run queue is at its compile-time bound.
    ///
    /// Bounded queues are a deliberate choice: an unbounded queue can be
    /// used as a `DoS` amplifier against the kernel. The caller may retry on
    /// another CPU (work-stealing path) or back-pressure the task source.
    QueueFull,
    /// No task is registered under the given [`crate::TaskId`].
    ///
    /// Returned by [`crate::SchedulerPolicy::park`],
    /// [`crate::SchedulerPolicy::unpark`], and
    /// [`crate::SchedulerPolicy::exit`] when the supplied identifier has
    /// already exited or was never spawned.
    NoSuchTask,
    /// The task is not in a state that allows the requested transition.
    ///
    /// Example: calling `unpark` on a task that is already running, or
    /// `park` on a task that has exited. The state machine is documented
    /// in `docs/src/architecture/scheduler.md`.
    InvalidState,
    /// The requested CPU identifier is outside the configured range.
    NoSuchCpu,
}

impl fmt::Display for SchedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::QueueFull => "scheduler run queue full",
            Self::NoSuchTask => "no such task",
            Self::InvalidState => "task is not in a state that permits this transition",
            Self::NoSuchCpu => "no such cpu",
        };
        f.write_str(s)
    }
}

/// Result alias used throughout the scheduler.
pub type SchedResult<T> = Result<T, SchedError>;

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;
    use alloc::format;

    #[test]
    fn display_covers_every_variant() {
        // Iterating manually rather than using `strum` so the test
        // breaks loudly if a new variant lands without a `Display`
        // arm (docs in sync with behaviour).
        for v in [
            SchedError::QueueFull,
            SchedError::NoSuchTask,
            SchedError::InvalidState,
            SchedError::NoSuchCpu,
        ] {
            let s = format!("{v}");
            assert!(!s.is_empty(), "Display for {v:?} must not be empty");
        }
    }
}
