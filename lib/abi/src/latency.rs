//! Interactive-surface latency ABI: the per-thread frame budget a surface
//! declares, and the bounds the kernel's overrun report is held to.
//!
//! An interactive surface owes the user an answer within a frame. A thread
//! that declares a budget through [`crate::SyscallNumber::LATENCY_WATCH`] is
//! asking the kernel to notice when it does not, and to say *why* — which
//! syscall it was blocked in, for how long, and the call stack that led
//! there. The surface itself cannot answer that: by the time a loop observes
//! that an iteration overran, its stack has unwound and any backtrace it
//! takes names the detector rather than the culprit.
//!
//! # The span the budget applies to
//!
//! The kernel opens a span when the thread returns from an event wait and
//! closes it at the next one, so a surface pays no per-frame syscall and
//! cannot misreport its own span. A timed park *inside* a span
//! ([`crate::SyscallNumber::WAITSET_WAIT`] on a memberless set with a finite
//! timeout) does not close it: sleeping on the frame path is one of the
//! defects this exists to surface.
//!
//! The overrun is noticed at the kernel entry or exit that crosses the
//! budget, so the frame it reports is the one the thread is *inside* at that
//! moment — its user stack is frozen, and the walk names the call that spent
//! the budget rather than the loop that later observed the cost. A thread
//! that never leaves its syscall again is a wedge, not a pause, and belongs
//! to the liveness watchdogs that already cover it.
//!
//! # A diagnostic, not a control
//!
//! Declaring a budget grants nothing, changes no scheduling decision, and
//! affects no other thread, so the call needs no capability. It is also a
//! debug-image facility: a shippable image compiles the machinery out and
//! answers every call with `0`, so a surface reads back the budget actually
//! armed rather than branching on the image it runs in.

/// Default frame budget an interactive surface declares, in nanoseconds.
///
/// A quarter of a second is far past any frame period a display runs at, so
/// a crossing is a pause a user notices rather than a missed frame — which
/// keeps the report about genuine freezes and off the ordinary jitter of a
/// loaded machine. A diagnostic policy value, not a resource capacity.
pub const DEFAULT_FRAME_BUDGET_NS: u64 = 250_000_000;

/// Smallest frame budget the kernel will arm, in nanoseconds.
///
/// A fixed containment bound, not a capacity: the budget comes from
/// userland, and an arbitrarily small one would let a task turn every
/// syscall it makes into a log record. One millisecond is below any real
/// frame period and above the noise of a single syscall.
pub const MIN_FRAME_BUDGET_NS: u64 = 1_000_000;

/// Shortest interval between two overrun reports for one thread, in
/// nanoseconds.
///
/// The per-span latch already bounds a report to one per park-to-park span,
/// so this bounds the *rate* at which a thread that parks and wakes in a
/// tight cycle can produce them. A fixed containment bound on an
/// append-only log, not a capacity.
pub const MIN_REPORT_INTERVAL_NS: u64 = 1_000_000_000;

/// Maximum number of user stack frames one overrun report carries.
///
/// A record bound, not a capacity: the report is one log line a developer
/// reads, and the frames nearest the stall are the ones that name it. The
/// shared unwinder's own cap bounds the walk itself.
pub const MAX_STALL_FRAMES: usize = 16;

/// The default budget must clear the armable floor, and must sit well past
/// any real frame period so ordinary jitter on a loaded machine can never
/// reach it — 250 ms is over thirty times a 144 Hz frame.
const _: () = assert!(DEFAULT_FRAME_BUDGET_NS > MIN_FRAME_BUDGET_NS);
const _: () = assert!(DEFAULT_FRAME_BUDGET_NS > 30 * (1_000_000_000 / 144));

/// Budget argument that disarms the calling thread's frame budget.
///
/// Spelled rather than left implicit so a surface shutting down says so,
/// and so `0` cannot read as "the smallest budget you will take".
pub const BUDGET_DISARM: u64 = 0;

/// What an overrun report's program counter and backtrace actually name.
///
/// The report never implies an observation it did not make. Both framed
/// variants are captured at a kernel entry the thread is *still inside*, so
/// its user stack is frozen while the frame is taken and walked; they differ
/// in what the thread was doing when the budget went.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum StallSample {
    /// The frame captured entering the syscall that carried the span past
    /// its budget: the blocking call site.
    Blocking,
    /// The frame captured at the kernel entry that *followed* an overrun
    /// spent running in user mode: the code that was executing.
    Running,
    /// No frame — the port publishes none for this target.
    None,
}

impl StallSample {
    /// Stable, human-readable provenance for the report's `sampled` field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blocking => "blocking",
            Self::Running => "running",
            Self::None => "none",
        }
    }
}

/// Clamp a caller-supplied frame budget to the armable range.
///
/// [`BUDGET_DISARM`] passes through as `None` (disarm); anything else is
/// raised to [`MIN_FRAME_BUDGET_NS`] rather than refused, so a surface that
/// asks for an unreasonably tight budget still gets a working watch instead
/// of an error it would have to handle.
#[must_use]
pub const fn clamp_budget_ns(budget_ns: u64) -> Option<u64> {
    if budget_ns == BUDGET_DISARM {
        None
    } else if budget_ns < MIN_FRAME_BUDGET_NS {
        Some(MIN_FRAME_BUDGET_NS)
    } else {
        Some(budget_ns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_disarms_and_anything_else_arms() {
        assert_eq!(clamp_budget_ns(BUDGET_DISARM), None);
        assert_eq!(
            clamp_budget_ns(DEFAULT_FRAME_BUDGET_NS),
            Some(DEFAULT_FRAME_BUDGET_NS)
        );
    }

    #[test]
    fn a_budget_under_the_floor_is_raised_not_refused() {
        assert_eq!(clamp_budget_ns(1), Some(MIN_FRAME_BUDGET_NS));
        assert_eq!(
            clamp_budget_ns(MIN_FRAME_BUDGET_NS - 1),
            Some(MIN_FRAME_BUDGET_NS)
        );
        assert_eq!(
            clamp_budget_ns(MIN_FRAME_BUDGET_NS),
            Some(MIN_FRAME_BUDGET_NS)
        );
    }

    #[test]
    fn provenance_renders_distinctly() {
        assert_eq!(StallSample::Blocking.as_str(), "blocking");
        assert_eq!(StallSample::Running.as_str(), "running");
        assert_eq!(StallSample::None.as_str(), "none");
    }
}
