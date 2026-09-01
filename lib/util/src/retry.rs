//! A bounded, doubling one-shot schedule for retrying something that is not
//! available yet and has no readiness event to wait on.

/// A bounded, doubling one-shot schedule for retrying something that is not
/// available yet and has no readiness event to wait on.
///
/// Several boot-order problems are the same problem — no userland event says
/// "it is there now" — so they climb this one definition rather than each
/// carrying a schedule of its own: the clock service's configuration store and
/// RTC, and the service manager's administrator enrolment overrides, which all
/// live behind a volume or a driver that appears after the reader starts.
///
/// It is a one-shot ladder, never a poll loop: the caller parks until [`at`]
/// and takes a single attempt, so a core is never pegged and a boot on which
/// the thing never appears is bounded by the ladder's own finite length.
///
/// Why a failed attempt never disarms it: every way the thing can be missing
/// looks the same from the caller. Reading one wrong is what strands a
/// service, so the guess is not worth making.
///
/// [`at`]: Self::at
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RetryLadder {
    /// Absolute nanosecond deadline of the next attempt.
    pub at: u64,
    wait: u64,
    left: u32,
}

impl RetryLadder {
    /// The schedule to climb while `satisfied` is false, or [`None`] when
    /// there is nothing to wait for.
    #[must_use]
    pub fn arm(now: u64, base_nanos: u64, attempts: u32, satisfied: bool) -> Option<Self> {
        (!satisfied).then_some(Self {
            at: now.saturating_add(base_nanos),
            wait: base_nanos,
            left: attempts,
        })
    }

    /// Advance to the next rung, or report the ladder spent.
    pub fn advance(&mut self, now: u64) -> bool {
        self.left = self.left.saturating_sub(1);
        if self.left == 0 {
            return false;
        }
        self.wait = self.wait.saturating_mul(2);
        self.at = now.saturating_add(self.wait);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::RetryLadder;

    #[test]
    fn an_already_satisfied_caller_arms_nothing() {
        assert_eq!(RetryLadder::arm(0, 1_000, 6, true), None);
    }

    #[test]
    fn the_ladder_doubles_and_is_spent_after_its_attempts() {
        let mut ladder = RetryLadder::arm(0, 1_000, 3, false).expect("arms");
        assert_eq!(ladder.at, 1_000);
        assert!(ladder.advance(1_000));
        assert_eq!(ladder.at, 3_000);
        assert!(ladder.advance(3_000));
        assert_eq!(ladder.at, 7_000);
        // The third advance spends the last attempt.
        assert!(!ladder.advance(7_000));
    }

    #[test]
    fn a_one_attempt_ladder_is_spent_by_its_first_advance() {
        let mut ladder = RetryLadder::arm(5, 10, 1, false).expect("arms");
        assert_eq!(ladder.at, 15);
        assert!(!ladder.advance(15));
    }

    #[test]
    fn arithmetic_saturates_rather_than_overflowing() {
        // Overflow checks are on in every profile, so a late instant or a
        // huge base must saturate rather than panic.
        let mut ladder = RetryLadder::arm(u64::MAX, 1_000, 4, false).expect("arms");
        assert_eq!(ladder.at, u64::MAX);
        assert!(ladder.advance(u64::MAX));
        assert_eq!(ladder.at, u64::MAX);
        let mut wide = RetryLadder::arm(0, u64::MAX, 4, false).expect("arms");
        assert!(wide.advance(0));
        assert_eq!(wide.at, u64::MAX);
    }
}
