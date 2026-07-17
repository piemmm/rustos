//! Per-stream rate limiting for log ingress (SYSLOG §11).
//!
//! A machine must be protected from log-driven denial of service: a runaway or
//! hostile emitter that floods the journal must not be able to exhaust storage
//! or starve other work. The two non-privileged, high-volume streams —
//! `runtime` and `debug` ([`Stream::is_rate_limitable`]) — may therefore be
//! rate-limited, and records offered beyond the configured rate are dropped.
//! The four system-authority streams (`boot`/`security`/`audit`/`journal`) are
//! **never** gated here: an audit or security record that cannot be accepted
//! fails closed to the caller instead of being silently dropped.
//!
//! Dropping is never silent. Every drop is folded into a per-stream tally, and
//! once a reporting interval has elapsed the journal drains that tally into one
//! trusted `journal`-stream loss record naming the stream, the number of
//! records dropped, and the window they were dropped over — so a reader sees an
//! explicit, coalesced "N runtime records dropped in the last W" rather than an
//! unexplained gap, and a flood produces at most one loss record per interval
//! per stream rather than a second flood of loss records.
//!
//! The limiter is a token bucket kept in integer nanoseconds so there is no
//! floating point and no accrual-rounding drift: a bucket earns one token every
//! [`RateLimit`]-configured interval and holds up to a configured burst of
//! them. It is `Copy`, `no_std`, and allocation-free, so a [`crate::Journal`]
//! holds one directly with no allocator and no lock.

use tairix_abi::Duration64;

use crate::stream::Stream;

/// The rate-limitable streams, in the order this module indexes its per-stream
/// state. Kept in lockstep with [`Stream::is_rate_limitable`] by
/// [`gated_index`]; a unit test asserts the two agree.
const GATED_STREAMS: [Stream; 2] = [Stream::Runtime, Stream::Debug];

/// Number of rate-limitable streams — the width of the per-stream state arrays.
const GATED: usize = GATED_STREAMS.len();

/// Nanoseconds per second, for the token-bucket arithmetic.
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// The index into the per-stream state arrays for `stream`, or `None` if the
/// stream is not rate-limitable.
fn gated_index(stream: Stream) -> Option<usize> {
    // Mirrors `Stream::is_rate_limitable`: the two must never disagree, so the
    // predicate gates the lookup rather than the match standing alone.
    if !stream.is_rate_limitable() {
        return None;
    }
    GATED_STREAMS.iter().position(|&s| s == stream)
}

/// Convert a monotonic [`Duration64`] to a whole nanosecond count, saturating.
///
/// Monotonic readings are non-negative and, as a since-boot span, fit a `u64`
/// nanosecond count for centuries; a pathological value saturates rather than
/// wrapping so the bucket arithmetic stays monotonic.
fn to_nanos(d: Duration64) -> u64 {
    let secs = u64::try_from(d.secs()).unwrap_or(0);
    secs.saturating_mul(NANOS_PER_SEC)
        .saturating_add(u64::from(d.subsec_nanos()))
}

/// A rate policy: a sustained rate plus a burst allowance, for one stream.
///
/// Expressed as a token bucket. One token is spent per admitted record; a token
/// is earned every [`Self`]-configured interval (`cost_nanos`), and the bucket
/// holds at most `capacity` tokens, so up to `capacity` records may arrive
/// back-to-back before the sustained rate applies.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RateLimit {
    /// Nanoseconds of accrual that buy one token (the inverse of the sustained
    /// rate). Never zero.
    cost_nanos: u64,
    /// Maximum credit the bucket holds, in nanoseconds: `capacity * cost_nanos`.
    capacity_nanos: u64,
}

impl RateLimit {
    /// A policy of `rate_per_sec` sustained records per second with a burst of
    /// `burst` records.
    ///
    /// `rate_per_sec` and `burst` are both clamped to at least one, so a
    /// zero-rate or zero-burst policy still admits at a floor rather than
    /// deadlocking a stream (a policy that admits *nothing* is not a rate
    /// limit; a caller wanting to disable a stream does so elsewhere).
    #[must_use]
    pub const fn per_second(rate_per_sec: u32, burst: u32) -> Self {
        let rate = if rate_per_sec == 0 { 1 } else { rate_per_sec };
        let burst = if burst == 0 { 1 } else { burst };
        let cost_nanos = NANOS_PER_SEC / rate as u64;
        // `rate <= 1_000_000_000`, so `cost_nanos >= 1`; guard anyway so the
        // capacity below and the admit arithmetic can never divide or scale by
        // zero.
        let cost_nanos = if cost_nanos == 0 { 1 } else { cost_nanos };
        Self {
            cost_nanos,
            capacity_nanos: cost_nanos.saturating_mul(burst as u64),
        }
    }
}

/// One stream's live token bucket.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Bucket {
    limit: RateLimit,
    /// Accrued credit, in nanoseconds, capped at `limit.capacity_nanos`.
    credit_nanos: u64,
    /// The monotonic time (nanoseconds) credit was last accrued to.
    last_nanos: u64,
    /// Whether [`Self::last_nanos`] has been seeded from a real reading yet.
    seeded: bool,
}

impl Bucket {
    const fn new(limit: RateLimit) -> Self {
        Self {
            limit,
            // Start full so the first burst is admitted immediately.
            credit_nanos: limit.capacity_nanos,
            last_nanos: 0,
            seeded: false,
        }
    }

    /// Accrue credit up to `now` and try to spend one token. Returns whether a
    /// token was available (the record is admitted).
    fn take(&mut self, now: u64) -> bool {
        if !self.seeded {
            self.last_nanos = now;
            self.seeded = true;
        }
        // Monotonic time only advances; a non-advancing reading accrues nothing
        // rather than draining credit.
        let elapsed = now.saturating_sub(self.last_nanos);
        self.last_nanos = now;
        self.credit_nanos = self
            .credit_nanos
            .saturating_add(elapsed)
            .min(self.limit.capacity_nanos);
        if self.credit_nanos >= self.limit.cost_nanos {
            self.credit_nanos -= self.limit.cost_nanos;
            true
        } else {
            false
        }
    }
}

/// The accumulated drops for one stream since the last report.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Tally {
    /// Records dropped in the current window; zero means no drop is pending.
    count: u64,
    /// The monotonic time (nanoseconds) of the first drop of the current
    /// window — the window's start. Meaningful only when `count > 0`.
    since_nanos: u64,
}

impl Tally {
    const fn empty() -> Self {
        Self {
            count: 0,
            since_nanos: 0,
        }
    }
}

/// The outcome of offering one record to the limiter.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RateDecision {
    /// The record is within the rate and may be committed.
    Admit,
    /// The record is over the rate and was dropped; a loss report is (or will
    /// become) due for its stream.
    Drop,
}

/// A coalesced drop report drained from the limiter, for one stream.
///
/// The journal turns this into one trusted `journal`-stream loss record.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DropReport {
    /// The rate-limited stream whose records were dropped.
    pub stream: Stream,
    /// The number of records dropped in the window.
    pub count: u64,
    /// The span from the first drop to the moment the report was drained.
    pub window: Duration64,
}

/// A per-stream token-bucket rate limiter for the rate-limitable streams.
///
/// Construct one with [`Self::new`] for a real policy, or [`Self::unlimited`]
/// for a limiter that never drops (the default a [`crate::Journal`] carries
/// until a policy is configured). Offer each record to [`Self::admit`], and
/// drain matured drops with [`Self::take_due_report`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RateLimiter {
    /// `None` for an unlimited limiter (never drops); `Some` per gated stream.
    buckets: Option<[Bucket; GATED]>,
    tallies: [Tally; GATED],
    /// Minimum time between loss reports for a stream, in nanoseconds, so a
    /// sustained flood coalesces into at most one loss record per interval.
    report_interval_nanos: u64,
}

impl RateLimiter {
    /// A limiter that never drops any record.
    ///
    /// This is the default: a journal with no configured policy does not
    /// rate-limit. The production journal service installs a real policy with
    /// [`Self::new`].
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            buckets: None,
            tallies: [Tally::empty(); GATED],
            report_interval_nanos: 0,
        }
    }

    /// A limiter with a policy per rate-limitable stream and a reporting
    /// interval.
    ///
    /// * `runtime` / `debug` — the token-bucket policy for each stream.
    /// * `report_interval` — the minimum time between coalesced loss reports
    ///   for a stream, so a sustained flood produces at most one loss record
    ///   per interval per stream.
    #[must_use]
    pub fn new(runtime: RateLimit, debug: RateLimit, report_interval: Duration64) -> Self {
        // The per-stream policies are indexed exactly as `GATED_STREAMS`.
        let buckets = [Bucket::new(runtime), Bucket::new(debug)];
        Self {
            buckets: Some(buckets),
            tallies: [Tally::empty(); GATED],
            report_interval_nanos: to_nanos(report_interval),
        }
    }

    /// Offer one record on `stream` at monotonic time `now`.
    ///
    /// A non-rate-limitable stream, and every stream under an
    /// [`unlimited`](Self::unlimited) limiter, is always admitted. A
    /// rate-limitable stream is admitted only if its bucket has a token;
    /// otherwise the record is dropped and folded into the stream's tally.
    pub fn admit(&mut self, stream: Stream, now: Duration64) -> RateDecision {
        let Some(buckets) = self.buckets.as_mut() else {
            return RateDecision::Admit;
        };
        let Some(i) = gated_index(stream) else {
            return RateDecision::Admit;
        };
        let now_nanos = to_nanos(now);
        if buckets[i].take(now_nanos) {
            RateDecision::Admit
        } else {
            let tally = &mut self.tallies[i];
            if tally.count == 0 {
                tally.since_nanos = now_nanos;
            }
            tally.count = tally.count.saturating_add(1);
            RateDecision::Drop
        }
    }

    /// Whether any stream has dropped records not yet reported.
    #[must_use]
    pub fn has_pending_drops(&self) -> bool {
        self.tallies.iter().any(|t| t.count > 0)
    }

    /// Drain `stream`'s drop tally into a [`DropReport`] if it is due at `now`.
    ///
    /// A report is due once at least the configured reporting interval has
    /// elapsed since the window's first drop, so drops coalesce into one report
    /// per interval. Draining resets the stream's window. Returns `None` when
    /// the stream is not rate-limitable, has no pending drops, or the interval
    /// has not yet elapsed.
    pub fn take_due_report(&mut self, stream: Stream, now: Duration64) -> Option<DropReport> {
        let i = gated_index(stream)?;
        let tally = &mut self.tallies[i];
        if tally.count == 0 {
            return None;
        }
        let now_nanos = to_nanos(now);
        let elapsed = now_nanos.saturating_sub(tally.since_nanos);
        if elapsed < self.report_interval_nanos {
            return None;
        }
        let count = tally.count;
        *tally = Tally::empty();
        Some(DropReport {
            stream,
            count,
            window: Duration64::from_nanos(elapsed),
        })
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::unlimited()
    }
}

#[cfg(test)]
mod tests {
    use super::{gated_index, DropReport, RateDecision, RateLimit, RateLimiter, GATED_STREAMS};
    use crate::stream::Stream;
    use tairix_abi::Duration64;

    fn at(ns: u64) -> Duration64 {
        Duration64::from_nanos(ns)
    }

    #[test]
    fn gated_index_matches_the_stream_predicate() {
        for s in Stream::ALL {
            assert_eq!(gated_index(s).is_some(), s.is_rate_limitable());
        }
        // The index order is exactly `GATED_STREAMS`.
        for (i, &s) in GATED_STREAMS.iter().enumerate() {
            assert_eq!(gated_index(s), Some(i));
        }
    }

    #[test]
    fn unlimited_never_drops() {
        let mut rl = RateLimiter::unlimited();
        for i in 0..10_000u64 {
            assert_eq!(rl.admit(Stream::Runtime, at(i)), RateDecision::Admit);
            assert_eq!(rl.admit(Stream::Debug, at(i)), RateDecision::Admit);
        }
        assert!(!rl.has_pending_drops());
    }

    #[test]
    fn non_rate_limitable_streams_are_always_admitted() {
        // A tight policy that would drop everything on a gated stream must not
        // touch the system-authority streams at all.
        let mut rl = RateLimiter::new(
            RateLimit::per_second(1, 1),
            RateLimit::per_second(1, 1),
            at(1),
        );
        for s in [
            Stream::Boot,
            Stream::Security,
            Stream::Audit,
            Stream::Journal,
        ] {
            for _ in 0..100 {
                assert_eq!(rl.admit(s, at(0)), RateDecision::Admit);
            }
        }
        assert!(!rl.has_pending_drops(), "no gated drops were recorded");
    }

    #[test]
    fn burst_is_admitted_then_excess_is_dropped() {
        // 10/s sustained, burst of 5: five back-to-back at t=0 pass, the sixth
        // (no time elapsed, bucket empty) is dropped.
        let mut rl = RateLimiter::new(
            RateLimit::per_second(10, 5),
            RateLimit::per_second(10, 5),
            at(1),
        );
        for _ in 0..5 {
            assert_eq!(rl.admit(Stream::Runtime, at(0)), RateDecision::Admit);
        }
        assert_eq!(rl.admit(Stream::Runtime, at(0)), RateDecision::Drop);
        assert!(rl.has_pending_drops());
    }

    #[test]
    fn credit_refills_over_time() {
        // 10/s = one token every 100ms; burst 1. After spending the initial
        // token, a record 100ms later is admitted again.
        let mut rl = RateLimiter::new(
            RateLimit::per_second(10, 1),
            RateLimit::per_second(10, 1),
            at(1),
        );
        assert_eq!(rl.admit(Stream::Runtime, at(0)), RateDecision::Admit);
        assert_eq!(rl.admit(Stream::Runtime, at(0)), RateDecision::Drop);
        // 100ms later one token has accrued.
        assert_eq!(
            rl.admit(Stream::Runtime, at(100_000_000)),
            RateDecision::Admit
        );
    }

    #[test]
    fn streams_are_limited_independently() {
        let mut rl = RateLimiter::new(
            RateLimit::per_second(10, 1),
            RateLimit::per_second(10, 1),
            at(1),
        );
        // Exhaust runtime; debug still has its own token.
        assert_eq!(rl.admit(Stream::Runtime, at(0)), RateDecision::Admit);
        assert_eq!(rl.admit(Stream::Runtime, at(0)), RateDecision::Drop);
        assert_eq!(rl.admit(Stream::Debug, at(0)), RateDecision::Admit);
    }

    #[test]
    fn a_report_is_due_only_after_the_interval_and_coalesces_drops() {
        // burst 1, report interval 1s.
        let mut rl = RateLimiter::new(
            RateLimit::per_second(1000, 1),
            RateLimit::per_second(1000, 1),
            at(1_000_000_000),
        );
        // Spend the token, then drop three records at the same instant — the
        // window starts at the first drop (t=0).
        assert_eq!(rl.admit(Stream::Runtime, at(0)), RateDecision::Admit);
        for _ in 0..3 {
            assert_eq!(rl.admit(Stream::Runtime, at(0)), RateDecision::Drop);
        }
        // Before the interval elapses, no report is due.
        assert_eq!(rl.take_due_report(Stream::Runtime, at(500_000_000)), None);
        // Once the full interval has elapsed since the first drop, one report
        // coalesces all three drops.
        let report = rl
            .take_due_report(Stream::Runtime, at(1_000_000_000))
            .expect("report due");
        assert_eq!(
            report,
            DropReport {
                stream: Stream::Runtime,
                count: 3,
                window: Duration64::from_nanos(1_000_000_000),
            }
        );
        // Draining resets the window: nothing pending now.
        assert!(!rl.has_pending_drops());
        assert_eq!(rl.take_due_report(Stream::Runtime, at(2_000_000_000)), None);
    }

    #[test]
    fn take_due_report_ignores_non_rate_limitable_streams() {
        let mut rl = RateLimiter::new(
            RateLimit::per_second(1, 1),
            RateLimit::per_second(1, 1),
            at(0),
        );
        assert_eq!(rl.take_due_report(Stream::Audit, at(10)), None);
        assert_eq!(rl.take_due_report(Stream::Journal, at(10)), None);
    }

    #[test]
    fn a_zero_rate_policy_still_admits_at_a_floor() {
        // `per_second(0, 0)` clamps to a 1/s, burst-1 policy rather than
        // deadlocking the stream.
        let mut rl = RateLimiter::new(
            RateLimit::per_second(0, 0),
            RateLimit::per_second(0, 0),
            at(1),
        );
        assert_eq!(rl.admit(Stream::Runtime, at(0)), RateDecision::Admit);
    }
}
