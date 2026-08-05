//! Pure next-deadline computation: the sample period, the read-cadence
//! policy every sysinfo-backed reading is classified under, and drift-free
//! deadline advancement, so the run loop's wait timeout is always exactly
//! "time until the next thing that must happen" — one wait per iteration,
//! never a busy poll.

/// How often the sampler gathers a fresh [`crate::sample::Sample`], in
/// nanoseconds.
///
/// Two seconds: frequent enough that the tray icon reads as live, sparse
/// enough that the ungated per-sample queries (the process list, the
/// aggregate CPU-time totals) stay a negligible fraction of system load.
pub const SAMPLE_PERIOD_NS: u64 = 2_000_000_000;

/// Every how many samples the audited memory-pressure query (and the other
/// kernel-internal operational reads that share its capability — see
/// [`crate::sample::ScopeVerdicts::memory_pressure`]) is issued when
/// granted, bounding its rate independently of the sample period.
///
/// Five samples at the [`SAMPLE_PERIOD_NS`] cadence is a ten-second memory
/// cadence: sparse enough that the audit log is not driven by a service
/// polling every two seconds, frequent enough that the tray's memory
/// pressure signal never lags a real transition by more than that window.
pub const MEMORY_SAMPLE_DIVIDER: u64 = 5;

/// Every how many samples the slow-moving system inventory queries are
/// issued: the mount table, per-volume I/O health, the seat list, resource
/// limits, and crash records.
///
/// Each of these changes only on an infrequent event — a volume mounted or
/// removed, a seat leased, a limit adjusted, a task killed by a fault —
/// never from moment to moment, so reading them on the fast per-sample
/// cadence would only add query volume with nothing fresher to show.
/// Three times the [`MEMORY_SAMPLE_DIVIDER`] cadence (thirty seconds at the
/// [`SAMPLE_PERIOD_NS`] rate) keeps the inventory current well inside the
/// time a user spends looking at the panel, without adding five more
/// queries to every tick.
pub const INVENTORY_SAMPLE_DIVIDER: u64 = MEMORY_SAMPLE_DIVIDER * 3;

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
pub(crate) fn advance_deadline(previous_deadline_ns: u64, now_ns: u64) -> u64 {
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
pub(crate) fn wait_timeout_ns(next_deadline_ns: u64, now_ns: u64) -> u64 {
    next_deadline_ns.saturating_sub(now_ns).max(MIN_WAIT_NS)
}

/// Whether a periodic reading gated by `divider` (see
/// [`MEMORY_SAMPLE_DIVIDER`] and [`INVENTORY_SAMPLE_DIVIDER`]) is due on
/// `sample_index`.
///
/// Sample `0` is always due, so a freshly started sampler's very first
/// sample already carries every periodic reading it is allowed to read,
/// rather than showing an empty state until the divider first comes
/// around. A `divider` of `0` is never due (there is no periodic reading
/// with a zero-sample cadence; this only guards the arithmetic).
#[must_use]
pub(crate) const fn periodic_due(sample_index: u64, divider: u64) -> bool {
    divider != 0 && sample_index.is_multiple_of(divider)
}

/// Which cadence tier one of the sampler's System Information API readings
/// belongs to.
///
/// Every reading [`crate::sample::Sampler::sample`] gathers is classified
/// into exactly one of these tiers, so the policy governing how often it is
/// actually queried is one documented definition here rather than a
/// modulus check re-invented at each call site in `sample.rs`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Cadence {
    /// The reading changes moment to moment, so it is issued on every
    /// sample: the process list, CPU time, per-CPU scheduler load, uptime,
    /// the load average, and live network interface state and throughput.
    EverySample,
    /// The existing audited memory-pressure cadence
    /// ([`MEMORY_SAMPLE_DIVIDER`]), also covering the other
    /// kernel-internal operational reads that share its capability (see
    /// [`crate::sample::ScopeVerdicts::memory_pressure`]).
    Memory,
    /// The slow-moving system inventory cadence
    /// ([`INVENTORY_SAMPLE_DIVIDER`]): the mount table, per-volume I/O
    /// health, the seat list, resource limits, and crash records.
    Inventory,
    /// Fetched once and cached for the sampler's life, re-fetched only
    /// when the previous attempt left the fact unavailable — the
    /// underlying fact (system identity, per-CPU model/class, installed
    /// RAM, and the set of network interfaces) cannot change while the
    /// sampling process runs.
    Static,
}

impl Cadence {
    /// Whether a reading in this tier should be (re)fetched on
    /// `sample_index`.
    ///
    /// `cached` is whether a value is already held for a
    /// [`Cadence::Static`] reading; it is ignored by every other tier; a
    /// tier value is always due exactly when a fresh reading offers new
    /// information.
    #[must_use]
    pub const fn due(self, sample_index: u64, cached: bool) -> bool {
        match self {
            Self::EverySample => true,
            Self::Memory => periodic_due(sample_index, MEMORY_SAMPLE_DIVIDER),
            Self::Inventory => periodic_due(sample_index, INVENTORY_SAMPLE_DIVIDER),
            Self::Static => !cached,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        advance_deadline, periodic_due, wait_timeout_ns, Cadence, INVENTORY_SAMPLE_DIVIDER,
        MEMORY_SAMPLE_DIVIDER, MIN_WAIT_NS, SAMPLE_PERIOD_NS,
    };

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

    #[test]
    fn periodic_due_is_true_on_the_first_sample() {
        assert!(periodic_due(0, MEMORY_SAMPLE_DIVIDER));
        assert!(periodic_due(0, INVENTORY_SAMPLE_DIVIDER));
    }

    #[test]
    fn periodic_due_is_true_only_on_exact_multiples() {
        for i in 0..3 * MEMORY_SAMPLE_DIVIDER {
            assert_eq!(
                periodic_due(i, MEMORY_SAMPLE_DIVIDER),
                i % MEMORY_SAMPLE_DIVIDER == 0
            );
        }
    }

    #[test]
    fn periodic_due_with_a_zero_divider_is_never_due() {
        assert!(!periodic_due(0, 0));
        assert!(!periodic_due(100, 0));
    }

    #[test]
    fn inventory_divider_is_a_multiple_of_the_memory_divider() {
        // The inventory cadence is deliberately derived from the memory
        // cadence rather than a second hand-picked constant.
        assert_eq!(INVENTORY_SAMPLE_DIVIDER % MEMORY_SAMPLE_DIVIDER, 0);
    }

    #[test]
    fn cadence_every_sample_is_always_due() {
        assert!(Cadence::EverySample.due(0, false));
        assert!(Cadence::EverySample.due(1, false));
        assert!(Cadence::EverySample.due(u64::MAX, true));
    }

    #[test]
    fn cadence_memory_follows_the_memory_divider() {
        for i in 0..3 * MEMORY_SAMPLE_DIVIDER {
            assert_eq!(
                Cadence::Memory.due(i, false),
                periodic_due(i, MEMORY_SAMPLE_DIVIDER)
            );
        }
    }

    #[test]
    fn cadence_inventory_follows_the_inventory_divider() {
        for i in 0..3 * INVENTORY_SAMPLE_DIVIDER {
            assert_eq!(
                Cadence::Inventory.due(i, false),
                periodic_due(i, INVENTORY_SAMPLE_DIVIDER)
            );
        }
    }

    #[test]
    fn cadence_static_is_due_only_when_uncached() {
        assert!(Cadence::Static.due(0, false));
        assert!(Cadence::Static.due(1_000, false));
        assert!(!Cadence::Static.due(0, true));
        assert!(!Cadence::Static.due(1_000, true));
    }
}
