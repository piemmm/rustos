//! Retry schedules: [`RetryLadder`] for waiting on something that has not
//! appeared yet, [`RestartPacer`] for restarting something that keeps dying.

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

/// A capped, doubling delay between restarts of something that keeps
/// failing, forgotten once a restart stays up for a stable window.
///
/// A crash loop is not a boot-order wait: what fails may be killed on
/// purpose — a parser a crafted packet brings down — so restarting it at
/// once would hand the sender a process spawn per packet. The service
/// manager and the parser sandbox's supervised worker pace their restarts
/// through this one definition.
///
/// Times are absolute monotonic nanoseconds, as [`RetryLadder`]'s are.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RestartPacer {
    base: u64,
    cap: u64,
    stable: u64,
    taken: u32,
    started_at: Option<u64>,
}

/// What [`RestartPacer::failed`] decided about one failure.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Restart {
    /// Restarts already taken since the paced thing last ran stably, not
    /// counting this one: what a caller with a crash-loop budget compares
    /// its budget against.
    pub taken: u32,
    /// The absolute instant the next start may be attempted.
    pub at: u64,
}

impl RestartPacer {
    /// A pacer whose first restart waits `base_nanos`, doubling with each
    /// consecutive failure up to `cap_nanos`, and which forgets those
    /// failures once a restart has stayed up for `stable_nanos`.
    #[must_use]
    pub const fn new(base_nanos: u64, cap_nanos: u64, stable_nanos: u64) -> Self {
        Self {
            base: base_nanos,
            cap: cap_nanos,
            stable: stable_nanos,
            taken: 0,
            started_at: None,
        }
    }

    /// Record that the paced thing was restarted at `now`.
    pub fn started(&mut self, now: u64) {
        self.started_at = Some(now);
    }

    /// Pace a failure at `now`.
    ///
    /// A restart that stayed up for the stable window — or a first run, which
    /// no restart preceded — clears the count first, so an isolated failure
    /// after a long healthy run pays nothing for failures long past.
    pub fn failed(&mut self, now: u64) -> Restart {
        if self
            .started_at
            .is_none_or(|at| now.saturating_sub(at) >= self.stable)
        {
            self.taken = 0;
        }
        let taken = self.taken;
        self.taken = self.taken.saturating_add(1);
        Restart {
            taken,
            at: now.saturating_add(self.delay(taken)),
        }
    }

    /// `base · 2^attempt`, clamped to the cap. A shift that would discard
    /// significant bits saturates rather than wrapping to a short delay.
    fn delay(&self, attempt: u32) -> u64 {
        let scaled = match self.base.checked_shl(attempt) {
            Some(value) if value >> attempt == self.base => value,
            _ => u64::MAX,
        };
        scaled.min(self.cap)
    }
}

#[cfg(test)]
mod tests {
    use super::{Restart, RestartPacer, RetryLadder};

    const MS: u64 = 1_000_000;

    #[test]
    fn restarts_double_from_the_base_and_clamp_to_the_cap() {
        let mut pacer = RestartPacer::new(100 * MS, 30_000 * MS, 30_000 * MS);
        pacer.started(0);
        assert_eq!(
            pacer.failed(0),
            Restart {
                taken: 0,
                at: 100 * MS
            }
        );
        assert_eq!(pacer.failed(0).at, 200 * MS);
        assert_eq!(pacer.failed(0).at, 400 * MS);
        for _ in 0..64 {
            let _ = pacer.failed(0);
        }
        // Past the shift width the delay saturates and is clamped, never
        // wrapped back to a short one.
        assert_eq!(pacer.failed(0).at, 30_000 * MS);
    }

    #[test]
    fn a_first_failure_and_a_stable_run_both_start_the_count_afresh() {
        let mut pacer = RestartPacer::new(100 * MS, 30_000 * MS, 1_000 * MS);
        // No restart preceded the first run.
        assert_eq!(pacer.failed(5 * MS).taken, 0);
        pacer.started(10 * MS);
        assert_eq!(pacer.failed(20 * MS).taken, 1);
        pacer.started(30 * MS);
        // Up for the whole stable window: an isolated failure.
        let restart = pacer.failed(30 * MS + 1_000 * MS);
        assert_eq!(restart.taken, 0);
        assert_eq!(restart.at, 30 * MS + 1_000 * MS + 100 * MS);
    }

    #[test]
    fn a_restart_that_fails_inside_the_window_keeps_counting() {
        let mut pacer = RestartPacer::new(MS, 1_000 * MS, 1_000 * MS);
        pacer.started(0);
        for expected in 0..5 {
            let restart = pacer.failed(10 * MS);
            assert_eq!(restart.taken, expected);
            pacer.started(restart.at);
        }
    }

    #[test]
    fn late_instants_saturate_rather_than_overflowing() {
        let mut pacer = RestartPacer::new(u64::MAX, u64::MAX, u64::MAX);
        pacer.started(u64::MAX);
        assert_eq!(pacer.failed(u64::MAX).at, u64::MAX);
    }

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
