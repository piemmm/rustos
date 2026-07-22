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
