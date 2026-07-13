//! Monotonic-deadline arithmetic, defined once.
//!
//! Several engines in this crate keep internal timers as a `u128`
//! nanosecond count so a deadline can be compared and offset without the
//! [`Duration64`] type needing addition or ordering. The widening, the
//! narrowing, and the "no deadline" sentinel live here so no two engines
//! carry their own copy of the same three-line conversion.

use rustos_abi::time::{Duration64, NANOS_PER_SEC};

/// Deadline value meaning "no timed transition is pending".
pub(crate) const NEVER: u128 = u128::MAX;

/// Widen a non-negative monotonic [`Duration64`] to nanoseconds.
///
/// A negative input (which monotonic time never produces) saturates to
/// zero rather than wrapping, so deadline arithmetic can never underflow.
pub(crate) fn nanos(d: Duration64) -> u128 {
    let secs = u128::try_from(d.secs()).unwrap_or(0);
    secs * u128::from(NANOS_PER_SEC) + u128::from(d.subsec_nanos())
}

/// Narrow an internal nanosecond deadline back to a [`Duration64`],
/// saturating a value beyond the `u64` monotonic range.
pub(crate) fn from_nanos(deadline: u128) -> Duration64 {
    Duration64::from_nanos(u64::try_from(deadline).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_nanoseconds() {
        let d = Duration64::from_nanos(1_500_000_123);
        assert_eq!(from_nanos(nanos(d)), d);
    }

    #[test]
    fn negative_saturates_to_zero() {
        assert_eq!(nanos(Duration64::from_secs(-5)), 0);
    }
}
