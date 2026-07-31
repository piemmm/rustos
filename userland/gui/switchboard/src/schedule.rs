//! Pure next-deadline computation: the sample period, the memory-query
//! cadence divider, and drift-free deadline advancement, so the run loop's
//! wait timeout is always exactly "time until the next thing that must
//! happen" — one wait per iteration, never a busy poll.

/// How often the sampler gathers a fresh [`crate::sample::Sample`], in
/// nanoseconds.
///
/// Two seconds: frequent enough that the tray icon reads as live, sparse
/// enough that the ungated per-sample queries (the process list, the
/// aggregate CPU-time totals) stay a negligible fraction of system load.
pub const SAMPLE_PERIOD_NS: u64 = 2_000_000_000;

/// Every how many samples the audited memory-pressure query is issued
/// (when granted), bounding its rate independently of the sample period.
///
/// Five samples at the [`SAMPLE_PERIOD_NS`] cadence is a ten-second memory
/// cadence: sparse enough that the audit log is not driven by a service
/// polling every two seconds, frequent enough that the tray's memory
/// pressure signal never lags a real transition by more than that window.
pub const MEMORY_SAMPLE_DIVIDER: u64 = 5;

/// The minimum relative wait a single `waitset_wait` call is given, even
/// when the schedule is already overdue.
///
/// A deadline that has already passed (e.g. after a slow publish attempt)
/// still yields a strictly positive timeout rather than `0`, so the loop
/// never spins requesting an immediate re-wait — the kernel wait-set call
/// itself is the park point, and it always parks for at least this long.
pub const MIN_WAIT_NS: u64 = 1_000_000;

/// Advance the absolute next-sample deadline from `previous_deadline_ns`,
/// anchored to the schedule rather than to `now_ns`, so successive samples
/// land on a steady [`SAMPLE_PERIOD_NS`] cadence instead of drifting later
/// by however long each iteration's work took.
///
/// If the schedule has fallen behind (`previous_deadline_ns` is already at
/// or before `now_ns` — a slow publish attempt, a delayed wake), the
/// deadline resyncs to one period after `now_ns` rather than firing a burst
/// of immediate catch-up samples.
#[must_use]
pub fn advance_deadline(previous_deadline_ns: u64, now_ns: u64) -> u64 {
    let next = previous_deadline_ns.saturating_add(SAMPLE_PERIOD_NS);
    if next > now_ns {
        next
    } else {
        now_ns.saturating_add(SAMPLE_PERIOD_NS)
    }
}

/// The relative timeout, in nanoseconds, a `waitset_wait` call should be
/// given to park until `next_deadline_ns`, floored at [`MIN_WAIT_NS`] so an
/// already-overdue deadline never yields a zero-length (spinning) wait.
#[must_use]
pub fn wait_timeout_ns(next_deadline_ns: u64, now_ns: u64) -> u64 {
    next_deadline_ns.saturating_sub(now_ns).max(MIN_WAIT_NS)
}

#[cfg(test)]
mod tests {
    use super::{advance_deadline, wait_timeout_ns, MIN_WAIT_NS, SAMPLE_PERIOD_NS};

    #[test]
    fn advance_deadline_steps_by_one_period_on_schedule() {
        assert_eq!(advance_deadline(1_000, 1_500), 1_000 + SAMPLE_PERIOD_NS);
    }

    #[test]
    fn advance_deadline_resyncs_when_overdue() {
        // The previous deadline has already passed `now_ns`: resync to one
        // period after now rather than a burst of immediate catch-ups.
        let previous = 1_000;
        let now = previous + SAMPLE_PERIOD_NS + 5_000_000_000;
        assert_eq!(advance_deadline(previous, now), now + SAMPLE_PERIOD_NS);
    }

    #[test]
    fn advance_deadline_resyncs_at_the_exact_boundary() {
        // `next == now` counts as "not ahead" (`next > now` is false), so
        // this also resyncs rather than yielding a zero-length wait.
        let previous = 0u64;
        let now = SAMPLE_PERIOD_NS;
        assert_eq!(advance_deadline(previous, now), now + SAMPLE_PERIOD_NS);
    }

    #[test]
    fn wait_timeout_is_the_remaining_time() {
        // Values comfortably above the `MIN_WAIT_NS` floor, so the
        // remaining time itself is returned.
        assert_eq!(wait_timeout_ns(10_000_000, 4_000_000), 6_000_000);
    }

    #[test]
    fn wait_timeout_floors_at_the_minimum_when_overdue() {
        assert_eq!(wait_timeout_ns(1_000, 5_000), MIN_WAIT_NS);
    }

    #[test]
    fn wait_timeout_saturates_rather_than_underflows() {
        assert_eq!(wait_timeout_ns(0, u64::MAX), MIN_WAIT_NS);
    }
}
