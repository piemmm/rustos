//! Unit tests and the shared conformance suite for TCP congestion control.
//!
//! The conformance suite (`conformance_*`) runs against *every*
//! [`CongestionAlgorithm`]: a new policy is correct only if it upholds the
//! RFC-mandated invariants here, exactly as a new scheduler must pass the
//! `kernel/sched/api` suite. The algorithm-specific tests then pin the
//! behaviour unique to `NewReno` and CUBIC.

use super::*;

const MSS: u32 = 1000;

/// Every shipped algorithm, for the conformance suite to iterate.
fn algorithms() -> [CongestionAlgorithm; 2] {
    [CongestionAlgorithm::Cubic, CongestionAlgorithm::NewReno]
}

/// Feed `total` bytes of cumulative ACKs in one-MSS steps at time `now_ns`,
/// modelling one round trip's worth of acknowledgements.
fn ack_window(cc: &mut dyn CongestionControl, total: u32, now_ns: u128) {
    let mut left = total;
    while left > 0 {
        let step = left.min(MSS);
        cc.on_ack(step, cc.cwnd(), now_ns);
        left -= step;
    }
}

#[test]
fn icbrt_is_the_exact_floor_of_the_cube_root() {
    assert_eq!(icbrt(0), 0);
    assert_eq!(icbrt(1), 1);
    assert_eq!(icbrt(7), 1);
    assert_eq!(icbrt(8), 2);
    assert_eq!(icbrt(26), 2);
    assert_eq!(icbrt(27), 3);
    assert_eq!(icbrt(1_000_000), 100);
    // Property: y³ ≤ v < (y+1)³ for a scattering of values, including the
    // 64-bit extreme (the cube of y+1 must be computed in u128).
    for v in [2u64, 63, 64, 999, 1 << 40, u64::MAX] {
        let y = u128::from(icbrt(v));
        let lo = y.pow(3);
        let hi = (y + 1).pow(3);
        assert!(lo <= u128::from(v), "{y}³ > {v}");
        assert!(hi > u128::from(v), "({y}+1)³ ≤ {v}");
    }
}

#[test]
fn conformance_initial_window_is_rfc6928() {
    // IW = min(10·MSS, max(2·MSS, 14600)); for MSS=1000 that is 10000.
    for algo in algorithms() {
        let cc = algo.build(MSS);
        assert_eq!(cc.cwnd(), 10_000, "{}", cc.name());
        assert!(cc.cwnd() >= MSS);
    }
}

#[test]
fn conformance_slow_start_grows_by_one_mss_per_ack() {
    for algo in algorithms() {
        let mut cc = algo.build(MSS);
        let start = cc.cwnd();
        // Below the (infinite) initial ssthresh, each MSS acked adds one MSS.
        for _ in 0..5 {
            cc.on_ack(MSS, cc.cwnd(), 0);
        }
        assert_eq!(cc.cwnd(), start + 5 * MSS, "{}", cc.name());
    }
}

#[test]
fn conformance_loss_reduces_the_window_and_sets_ssthresh() {
    for algo in algorithms() {
        let mut cc = algo.build(MSS);
        // Climb to a sizeable window first.
        for _ in 0..40 {
            cc.on_ack(MSS, cc.cwnd(), 0);
        }
        let before = cc.cwnd();
        cc.on_loss(before, 1_000_000);
        assert!(cc.cwnd() < before, "{}: no reduction", cc.name());
        assert!(cc.ssthresh() >= 2 * MSS, "{}", cc.name());
        assert_eq!(
            cc.cwnd(),
            cc.ssthresh(),
            "{}: cwnd deflates to ssthresh",
            cc.name()
        );
        assert!(cc.cwnd() >= MSS);
    }
}

#[test]
fn conformance_timeout_collapses_to_one_mss() {
    for algo in algorithms() {
        let mut cc = algo.build(MSS);
        for _ in 0..40 {
            cc.on_ack(MSS, cc.cwnd(), 0);
        }
        cc.on_rto(cc.cwnd(), 2_000_000);
        assert_eq!(cc.cwnd(), MSS, "{}: RTO must collapse to LW", cc.name());
        assert!(cc.ssthresh() >= 2 * MSS, "{}", cc.name());
    }
}

#[test]
fn conformance_congestion_avoidance_is_slower_than_slow_start() {
    for algo in algorithms() {
        let mut cc = algo.build(MSS);
        // One window of ACKs in slow start.
        let ss_start = cc.cwnd();
        ack_window(&mut *cc, ss_start, 0);
        let ss_growth = cc.cwnd() - ss_start;

        // Force congestion avoidance via a loss, then one window of ACKs.
        cc.on_loss(cc.cwnd(), 1_000_000);
        let ca_start = cc.cwnd();
        ack_window(&mut *cc, ca_start, 1_000_000);
        let ca_growth = cc.cwnd() - ca_start;

        assert!(
            ca_growth >= MSS,
            "{}: CA must make forward progress ({ca_growth})",
            cc.name()
        );
        assert!(
            ca_growth < ss_growth,
            "{}: CA ({ca_growth}) must be gentler than slow start ({ss_growth})",
            cc.name()
        );
    }
}

#[test]
fn conformance_window_never_shrinks_across_a_pure_ack_stream() {
    for algo in algorithms() {
        let mut cc = algo.build(MSS);
        cc.on_loss(20 * MSS, 1_000_000);
        let mut prev = cc.cwnd();
        let mut now = 1_000_000u128;
        for _ in 0..200 {
            now += 20_000_000; // 20 ms per RTT
            let w = cc.cwnd();
            ack_window(&mut *cc, w, now);
            assert!(cc.cwnd() >= prev, "{}: cwnd went backwards", cc.name());
            prev = cc.cwnd();
        }
    }
}

#[test]
fn cubic_is_never_less_aggressive_than_newreno() {
    // RFC 9438 §4.3 TCP-friendliness: over an identical loss + recovery
    // history CUBIC's window is at least NewReno's at every step.
    let mut cubic = Cubic::new(MSS);
    let mut reno = NewReno::new(MSS);
    // Identical slow-start climb.
    for _ in 0..30 {
        cubic.on_ack(MSS, cubic.cwnd(), 0);
        reno.on_ack(MSS, reno.cwnd(), 0);
    }
    // Identical loss.
    let flight = cubic.cwnd();
    cubic.on_loss(flight, 1_000_000);
    reno.on_loss(flight, 1_000_000);
    assert!(
        cubic.cwnd() >= reno.cwnd(),
        "cubic starts below reno after loss"
    );
    // Identical congestion-avoidance recovery.
    let mut now = 1_000_000u128;
    for _ in 0..300 {
        now += 20_000_000;
        let (wc, wr) = (cubic.cwnd(), reno.cwnd());
        ack_window(&mut cubic, wc, now);
        ack_window(&mut reno, wr, now);
        assert!(
            cubic.cwnd() >= reno.cwnd(),
            "cubic {} < reno {} at t={now}",
            cubic.cwnd(),
            reno.cwnd()
        );
    }
}

#[test]
fn cubic_convex_region_overshoots_the_prior_peak() {
    // Well past K the cubic term is positive, so the window climbs above the
    // pre-loss peak W_max (the bandwidth-probing convex region).
    let mut cubic = Cubic::new(MSS);
    for _ in 0..50 {
        cubic.on_ack(MSS, cubic.cwnd(), 0);
    }
    let w_max = cubic.cwnd();
    cubic.on_loss(w_max, 1_000_000);
    let mut now = 1_000_000u128;
    for _ in 0..2000 {
        now += 10_000_000;
        let w = cubic.cwnd();
        ack_window(&mut cubic, w, now);
    }
    assert!(
        cubic.cwnd() > w_max,
        "cubic {} never re-probed past W_max {w_max}",
        cubic.cwnd()
    );
}

/// Drive a policy from its initial slow-start window into congestion
/// avoidance: climb under slow start, take one loss to anchor a finite
/// `ssthresh`, then acknowledge several windows so `cwnd` rises above it.
fn drive_to_congestion_avoidance(cc: &mut dyn CongestionControl) {
    for _ in 0..30 {
        cc.on_ack(MSS, cc.cwnd(), 0);
    }
    cc.on_loss(cc.cwnd(), 1_000_000);
    let mut now = 1_000_000u128;
    for _ in 0..12 {
        now += 20_000_000;
        let w = cc.cwnd();
        ack_window(cc, w, now);
    }
}

#[test]
fn conformance_ecn_reduces_the_window_and_sets_ssthresh() {
    // RFC 3168 §6.1.2: an ECN mark shrinks the window like a loss (but with
    // no retransmission). Every policy must still deflate cwnd to ssthresh
    // and hold the 2·MSS floor.
    for algo in algorithms() {
        let mut cc = algo.build(MSS);
        drive_to_congestion_avoidance(&mut *cc);
        let before = cc.cwnd();
        cc.on_ecn(before, 500_000_000);
        assert!(cc.cwnd() < before, "{}: no ECN reduction", cc.name());
        assert!(cc.ssthresh() >= 2 * MSS, "{}", cc.name());
        assert_eq!(
            cc.cwnd(),
            cc.ssthresh(),
            "{}: cwnd deflates to ssthresh",
            cc.name()
        );
        assert!(cc.cwnd() >= MSS);
    }
}

#[test]
fn conformance_abe_ecn_backoff_is_gentler_than_loss_in_congestion_avoidance() {
    // RFC 8511 §3: in congestion avoidance an ECN mark backs off with a
    // larger multiplicative-decrease factor than a loss, so the resulting
    // window is strictly larger. Two instances driven identically diverge
    // only in the final signal.
    for algo in algorithms() {
        let mut via_ecn = algo.build(MSS);
        let mut via_loss = algo.build(MSS);
        drive_to_congestion_avoidance(&mut *via_ecn);
        drive_to_congestion_avoidance(&mut *via_loss);
        let flight = via_ecn.cwnd();
        assert!(flight > via_ecn.ssthresh(), "{}: not in CA", via_ecn.name());
        assert_eq!(flight, via_loss.cwnd(), "{}: not identical", via_ecn.name());

        via_ecn.on_ecn(flight, 500_000_000);
        via_loss.on_loss(flight, 500_000_000);
        assert!(
            via_ecn.cwnd() > via_loss.cwnd(),
            "{}: ABE ({}) must exceed loss backoff ({})",
            via_ecn.name(),
            via_ecn.cwnd(),
            via_loss.cwnd()
        );
    }
}

#[test]
fn conformance_abe_falls_back_to_loss_backoff_in_slow_start() {
    // RFC 8511 §3.1: ABE only applies in congestion avoidance. In slow
    // start (cwnd ≤ ssthresh) an ECN mark gets the standard loss reduction.
    for algo in algorithms() {
        let mut via_ecn = algo.build(MSS);
        let mut via_loss = algo.build(MSS);
        for _ in 0..5 {
            via_ecn.on_ack(MSS, via_ecn.cwnd(), 0);
            via_loss.on_ack(MSS, via_loss.cwnd(), 0);
        }
        let flight = via_ecn.cwnd();
        assert!(
            flight <= via_ecn.ssthresh(),
            "{}: expected slow start",
            via_ecn.name()
        );
        via_ecn.on_ecn(flight, 1_000_000);
        via_loss.on_loss(flight, 1_000_000);
        assert_eq!(via_ecn.cwnd(), via_loss.cwnd(), "{}", via_ecn.name());
        assert_eq!(
            via_ecn.ssthresh(),
            via_loss.ssthresh(),
            "{}",
            via_ecn.name()
        );
    }
}

#[test]
fn newreno_abe_backs_off_to_four_fifths_of_flight() {
    // NewReno beta_ecn = 0.8 vs beta_loss = 0.5 (RFC 8511 §3).
    let mut reno = NewReno::new(MSS);
    drive_to_congestion_avoidance(&mut reno);
    assert!(reno.cwnd() > reno.ssthresh());
    let flight = reno.cwnd();
    let mut ecn = reno;
    let mut loss = reno;
    ecn.on_ecn(flight, 3);
    loss.on_loss(flight, 3);
    assert_eq!(
        ecn.ssthresh(),
        u32::try_from(u64::from(flight) * 800 / 1000).unwrap()
    );
    assert_eq!(ecn.cwnd(), ecn.ssthresh());
    assert_eq!(loss.ssthresh(), flight / 2);
    assert!(ecn.cwnd() > loss.cwnd());
}

#[test]
fn cubic_abe_backs_off_to_beta_ecn_of_cwnd() {
    // CUBIC beta_ecn = 0.85 vs beta_loss = 0.7 (RFC 8511 §3.1).
    let mut cubic = Cubic::new(MSS);
    for _ in 0..40 {
        cubic.on_ack(MSS, cubic.cwnd(), 0);
    }
    cubic.on_loss(cubic.cwnd(), 1_000_000);
    let mut now = 1_000_000u128;
    for _ in 0..15 {
        now += 20_000_000;
        let w = cubic.cwnd();
        ack_window(&mut cubic, w, now);
    }
    assert!(cubic.cwnd() > cubic.ssthresh());
    let flight = cubic.cwnd();
    let mut ecn = cubic;
    let mut loss = cubic;
    ecn.on_ecn(flight, now);
    loss.on_loss(flight, now);
    assert_eq!(
        ecn.ssthresh(),
        u32::try_from(u64::from(flight) * 850 / 1000).unwrap()
    );
    assert_eq!(
        loss.ssthresh(),
        u32::try_from(u64::from(flight) * 700 / 1000).unwrap()
    );
    assert!(ecn.cwnd() > loss.cwnd());
}

#[test]
fn newreno_halves_on_loss() {
    let mut reno = NewReno::new(MSS);
    for _ in 0..40 {
        reno.on_ack(MSS, reno.cwnd(), 0);
    }
    let flight = reno.cwnd();
    reno.on_loss(flight, 0);
    assert_eq!(reno.ssthresh(), flight / 2);
    assert_eq!(reno.cwnd(), flight / 2);
}

#[test]
fn set_mss_keeps_the_byte_window() {
    for algo in algorithms() {
        let mut cc = algo.build(MSS);
        let cwnd = cc.cwnd();
        cc.set_mss(1400);
        assert_eq!(
            cc.cwnd(),
            cwnd,
            "{}: MSS change must not move cwnd",
            cc.name()
        );
    }
}

#[test]
fn init_resets_state() {
    for algo in algorithms() {
        let mut cc = algo.build(MSS);
        cc.on_rto(20 * MSS, 0);
        cc.init(MSS);
        assert_eq!(cc.cwnd(), 10_000, "{}", cc.name());
        assert_eq!(cc.ssthresh(), u32::MAX, "{}", cc.name());
    }
}
