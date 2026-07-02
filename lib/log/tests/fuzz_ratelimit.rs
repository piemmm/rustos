//! Deterministic fuzz-style integration test for the ingress rate limiter.
//!
//! A [`rustos_log::RateLimiter`] gates the two rate-limitable streams
//! (`runtime`/`debug`) and must never panic, never lose a drop silently, and
//! never touch a system-authority stream. This harness drives a random stream
//! of `admit` / `take_due_report` operations at non-decreasing monotonic times
//! against a limiter with a randomly-drawn policy, checking the load-bearing
//! invariants against a simple counting model:
//!
//! * a non-rate-limitable stream (`boot`/`security`/`audit`/`journal`) is
//!   always admitted and never produces a drop report;
//! * every drop is accounted: the sum of all reported `count`s, plus any drops
//!   still pending at the end, equals the number of `Drop` decisions made —
//!   no drop vanishes and none is double-counted;
//! * a report is only returned once at least the configured interval has
//!   elapsed since the window's first drop, and its `count` is non-zero and its
//!   `window` is at least that interval; and
//! * the limiter never panics on any operation ordering.
//!
//! Seed selection, the start-of-test seed log, and the smoke / soak loop are
//! the shared `rustos_fuzzseed` seam (one definition).

use rustos_abi::Duration64;
use rustos_log::{RateDecision, RateLimit, RateLimiter, Stream};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// A far-future instant used to force every pending drop to be reportable when
/// draining the model at the end of a run.
const FAR_FUTURE_NANOS: u64 = u64::MAX / 2;

fn stream_of(raw: u64) -> Stream {
    // `Stream::from_u8` fails closed above the closed set; map into range so a
    // valid stream is always produced.
    Stream::from_u8((raw % 6) as u8).expect("in-range discriminant")
}

/// Index of a rate-limitable stream in the per-stream model arrays, or `None`.
fn gated_index(stream: Stream) -> Option<usize> {
    match stream {
        Stream::Runtime => Some(0),
        Stream::Debug => Some(1),
        _ => None,
    }
}

#[test]
fn rate_limiter_accounts_every_drop_and_never_panics() {
    let mut prng = rustos_fuzzseed::Lcg::new(rustos_fuzzseed::start(
        "rate_limiter_accounts_every_drop_and_never_panics",
        rustos_fuzzseed::FUZZ_SEED_ENV,
    ));

    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            // Draw a policy: rate 1..=4096/s, burst 1..=256, report interval
            // 1ns..~1s. Both gated streams share the same drawn policy shape,
            // but keep independent buckets and tallies.
            let rate = 1 + (prng.next_u64() % 4096) as u32;
            let burst = 1 + (prng.next_u64() % 256) as u32;
            let report_interval_nanos = 1 + prng.next_u64() % 1_000_000_000;
            let mut rl = RateLimiter::new(
                RateLimit::per_second(rate, burst),
                RateLimit::per_second(rate, burst),
                Duration64::from_nanos(report_interval_nanos),
            );

            // Per-gated-stream counters: total Drop decisions, and total drops
            // handed back through reports.
            let mut dropped = [0u64; 2];
            let mut reported = [0u64; 2];

            let mut now: u64 = 0;
            let ops = prng.next_u64() % 256;
            for _ in 0..ops {
                // Time never goes backwards; occasionally it does not advance
                // at all (a burst at one instant).
                now = now.saturating_add(prng.next_u64() % 5_000_000);
                let stream = stream_of(prng.next_u64());

                if prng.next_u64() % 4 == 0 {
                    // A report request. A non-gated stream returns `None`; a
                    // gated one returns a report only once due — the
                    // `expect` below would fire if a non-gated stream ever
                    // returned a report.
                    if let Some(report) = rl.take_due_report(stream, Duration64::from_nanos(now)) {
                        let i = gated_index(stream).expect("only gated streams report");
                        assert_eq!(report.stream, stream, "report names its stream");
                        assert!(report.count > 0, "a report never coalesces zero drops");
                        let window = u64::try_from(report.window.secs()).unwrap_or(0)
                            * 1_000_000_000
                            + u64::from(report.window.subsec_nanos());
                        assert!(
                            window >= report_interval_nanos,
                            "a report only fires once the interval elapsed"
                        );
                        reported[i] += report.count;
                    }
                } else {
                    // An admit.
                    match rl.admit(stream, Duration64::from_nanos(now)) {
                        RateDecision::Admit => {}
                        RateDecision::Drop => {
                            let i = gated_index(stream)
                                .expect("only a rate-limitable stream is ever dropped");
                            dropped[i] += 1;
                        }
                    }
                }
            }

            // Drain every pending drop by asking far in the future, then the
            // conservation law must hold exactly for each gated stream.
            for (i, stream) in [Stream::Runtime, Stream::Debug].into_iter().enumerate() {
                if let Some(report) =
                    rl.take_due_report(stream, Duration64::from_nanos(FAR_FUTURE_NANOS))
                {
                    reported[i] += report.count;
                }
                assert_eq!(
                    reported[i], dropped[i],
                    "every drop on {stream:?} is reported exactly once"
                );
            }
            assert!(!rl.has_pending_drops(), "the drain reported all drops");
        }
        if !rustos_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
