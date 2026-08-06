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

/// The deadline to hold and the relative timeout, in nanoseconds, to park
/// until it — re-anchoring a deadline that is already at or behind `now_ns`.
///
/// The schedule is advanced from the clock reading taken *before* a cycle's
/// work, but the park happens after it, so a cycle whose sampling, model
/// rebuild, publish and repaint together cost a whole period leaves nothing
/// left to wait for. Parking for that remainder would re-enter the full cycle
/// at once, and again, and again: the sampler would free-run at whatever rate
/// it can complete a cycle instead of its nominal one per period. Skipping the
/// missed period instead is what bounds an expensive cycle's duty to the work
/// itself plus a full idle period, and is why the returned timeout is always
/// strictly positive.
///
/// The caller adopts the returned deadline, so the wait and the next cycle's
/// due-check are decided against the same reading rather than drifting apart.
#[must_use]
pub(crate) fn park_until(next_deadline_ns: u64, now_ns: u64) -> (u64, u64) {
    if let Some(remaining) = next_deadline_ns
        .checked_sub(now_ns)
        .filter(|left| *left > 0)
    {
        return (next_deadline_ns, remaining);
    }
    let deadline = advance_deadline(next_deadline_ns, now_ns);
    // `advance_deadline` lands strictly ahead of `now_ns` in every case but
    // one: a nanosecond clock at the top of its range, where the addition
    // saturates and no deadline can be ahead of anything. Parking a whole
    // period there keeps the loop parked instead of spinning on a schedule
    // that can no longer move.
    let remaining = deadline.saturating_sub(now_ns);
    (
        deadline,
        if remaining > 0 {
            remaining
        } else {
            SAMPLE_PERIOD_NS
        },
    )
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
        advance_deadline, park_until, periodic_due, Cadence, INVENTORY_SAMPLE_DIVIDER,
        MEMORY_SAMPLE_DIVIDER, SAMPLE_PERIOD_NS,
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
    fn park_until_waits_the_remaining_time_and_keeps_the_deadline() {
        assert_eq!(park_until(10_000_000, 4_000_000), (10_000_000, 6_000_000));
    }

    /// The regression this function exists for: a cycle that overran its
    /// deadline must still park, and the cadence stays on the period grid
    /// rather than drifting by the overrun.
    #[test]
    fn park_until_re_anchors_a_deadline_the_cycle_overran() {
        let deadline = SAMPLE_PERIOD_NS;
        let now = deadline + 1;
        let (next, timeout) = park_until(deadline, now);
        assert_eq!(next, deadline + SAMPLE_PERIOD_NS);
        assert_eq!(timeout, SAMPLE_PERIOD_NS - 1);
    }

    /// A cycle costing more than a whole period skips the period it missed
    /// rather than firing a burst of catch-up samples back to back.
    #[test]
    fn park_until_skips_a_wholly_missed_period() {
        let now = 10 * SAMPLE_PERIOD_NS;
        assert_eq!(
            park_until(SAMPLE_PERIOD_NS, now),
            (now + SAMPLE_PERIOD_NS, SAMPLE_PERIOD_NS)
        );
    }

    /// However far behind the schedule is — and however the deadline
    /// arithmetic saturates — the loop parks: a zero-length wait would turn
    /// the sampler into a busy poll of the whole cycle.
    #[test]
    fn park_until_never_returns_a_zero_wait() {
        for now in [
            0,
            1,
            SAMPLE_PERIOD_NS,
            SAMPLE_PERIOD_NS + 1,
            17 * SAMPLE_PERIOD_NS,
            u64::MAX - 1,
            u64::MAX,
        ] {
            for deadline in [0, 1, SAMPLE_PERIOD_NS, u64::MAX - 1, u64::MAX] {
                let (_, timeout) = park_until(deadline, now);
                assert!(timeout > 0, "deadline {deadline}, now {now}");
            }
        }
    }

    /// At the very top of the clock's range the deadline saturates, so the
    /// timeout — not the distance to it — is what keeps the loop parked.
    #[test]
    fn park_until_still_parks_when_the_deadline_saturates() {
        assert_eq!(park_until(u64::MAX, u64::MAX), (u64::MAX, SAMPLE_PERIOD_NS));
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
