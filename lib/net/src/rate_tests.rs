//! Unit tests for the windowed-rate meter.

use super::{
    RateCounters, RateMeter, RateSelector, HISTORY, MAX_WINDOW_NANOS, MIN_SAMPLE_GAP_NANOS,
};
use tairix_abi::time::Duration64;

/// Nanoseconds as a monotonic instant.
fn at(ns: u64) -> Duration64 {
    Duration64::from_nanos(ns)
}

/// The sampling gap as a `u64` (it fits comfortably).
fn gap_ns() -> u64 {
    u64::try_from(MIN_SAMPLE_GAP_NANOS).expect("gap fits u64")
}

/// The longest reportable window as a `u64`.
fn max_window_ns() -> u64 {
    u64::try_from(MAX_WINDOW_NANOS).expect("max window fits u64")
}

/// Counters carrying only a received-byte and received-packet total.
fn rx(packets: u64, bytes: u64) -> RateCounters {
    RateCounters {
        rx_packets: packets,
        rx_bytes: bytes,
        tx_packets: 0,
        tx_bytes: 0,
    }
}

#[test]
fn no_history_reports_zero_over_a_zero_window() {
    let meter = RateMeter::new();
    let reading = meter.rate(
        at(1_000_000_000),
        rx(100, 200_000),
        Duration64::from_secs(1),
        RateSelector::RxPackets,
    );
    assert_eq!(reading.value, 0);
    assert_eq!(reading.window, Duration64::ZERO);
}

#[test]
fn packet_rate_is_the_average_over_the_measured_window() {
    let mut meter = RateMeter::new();
    // Baseline at t=0, then read one second later with 1000 more packets.
    meter.record(at(0), rx(0, 0));
    let reading = meter.rate(
        at(1_000_000_000),
        rx(1000, 0),
        Duration64::from_secs(1),
        RateSelector::RxPackets,
    );
    assert_eq!(reading.value, 1000, "1000 packets in 1s = 1000 pps");
    assert_eq!(reading.window, Duration64::from_secs(1));
}

#[test]
fn bit_rate_multiplies_bytes_by_eight() {
    let mut meter = RateMeter::new();
    meter.record(at(0), rx(0, 0));
    // 1_000_000 bytes over 2 seconds = 500_000 B/s = 4_000_000 bit/s.
    let reading = meter.rate(
        at(2_000_000_000),
        rx(0, 1_000_000),
        Duration64::from_secs(2),
        RateSelector::RxBits,
    );
    assert_eq!(reading.value, 4_000_000);
    assert_eq!(reading.window, Duration64::from_secs(2));
}

#[test]
fn a_counter_that_goes_backwards_saturates_to_zero() {
    let mut meter = RateMeter::new();
    meter.record(at(0), rx(1000, 0));
    let reading = meter.rate(
        at(1_000_000_000),
        rx(10, 0),
        Duration64::from_secs(1),
        RateSelector::RxPackets,
    );
    assert_eq!(reading.value, 0);
}

#[test]
fn sub_gap_records_coalesce_and_keep_the_long_baseline() {
    let mut meter = RateMeter::new();
    // One old baseline, then a flood of sub-gap records that must not
    // evict it.
    meter.record(at(0), rx(0, 0));
    let mut t = 1;
    while t < gap_ns() {
        meter.record(at(t), rx(t, 0));
        t += 1_000_000; // 1 ms apart, all inside the first gap
    }
    // A one-second window still resolves against the t=0 baseline.
    let reading = meter.rate(
        at(1_000_000_000),
        rx(1000, 0),
        Duration64::from_secs(1),
        RateSelector::RxPackets,
    );
    assert_eq!(reading.value, 1000);
    assert_eq!(reading.window, Duration64::from_secs(1));
}

#[test]
fn window_shorter_than_history_picks_the_nearest_baseline() {
    let mut meter = RateMeter::new();
    // Snapshots every 250 ms for 2 s, 100 packets per snapshot.
    for step in 0..=8u64 {
        let t = step * gap_ns();
        meter.record(at(t), rx(step * 100, 0));
    }
    // Ask for a 500 ms window at t=2s (packet total 800): the baseline is
    // the snapshot at t=1.5s (600 packets), so 200 packets / 0.5 s.
    let reading = meter.rate(
        at(2_000_000_000),
        rx(800, 0),
        Duration64::from_nanos(500_000_000),
        RateSelector::RxPackets,
    );
    assert_eq!(reading.window, Duration64::from_nanos(500_000_000));
    assert_eq!(reading.value, 400, "200 packets in 0.5 s = 400 pps");
}

#[test]
fn window_beyond_history_reports_the_actual_shorter_span() {
    let mut meter = RateMeter::new();
    meter.record(at(1_000_000_000), rx(0, 0));
    // Request a window far longer than the single 1 s-old baseline covers.
    let reading = meter.rate(
        at(2_000_000_000),
        rx(500, 0),
        Duration64::from_secs(60),
        RateSelector::RxPackets,
    );
    // Measured over the honest 1 s span, not the requested 60 s.
    assert_eq!(reading.window, Duration64::from_secs(1));
    assert_eq!(reading.value, 500);
}

#[test]
fn the_ring_drops_the_oldest_when_full() {
    let mut meter = RateMeter::new();
    // Fill well past HISTORY so only the last HISTORY snapshots survive.
    for step in 0..(HISTORY as u64 * 2) {
        let t = step * gap_ns();
        meter.record(at(t), rx(step, 0));
    }
    let now = (HISTORY as u64 * 2 - 1) * gap_ns();
    // A window far longer than the ring can honour is clamped to the
    // surviving span: the oldest of the last HISTORY snapshots.
    let reading = meter.rate(
        at(now),
        rx(HISTORY as u64 * 2 - 1, 0),
        Duration64::from_secs(3600),
        RateSelector::RxPackets,
    );
    assert_eq!(
        reading.window,
        Duration64::from_nanos(max_window_ns()),
        "measured span is the surviving ring's oldest snapshot"
    );
    // Over the (HISTORY - 1) surviving gaps the packet total rose by
    // (HISTORY - 1), so the rate is exactly (HISTORY - 1) packets over
    // MAX_WINDOW_NANOS = 4 pps for the default resolution.
    let expected = (HISTORY as u128 - 1) * 1_000_000_000 / MAX_WINDOW_NANOS;
    assert_eq!(u128::from(reading.value), expected);
}

#[test]
fn a_non_advancing_record_never_corrupts_the_baseline() {
    let mut meter = RateMeter::new();
    meter.record(at(1_000_000_000), rx(100, 0));
    // A stray record with an earlier timestamp is ignored.
    meter.record(at(500_000_000), rx(9999, 0));
    let reading = meter.rate(
        at(2_000_000_000),
        rx(1100, 0),
        Duration64::from_secs(1),
        RateSelector::RxPackets,
    );
    // Baseline stays the 1 s-old (100-packet) snapshot: 1000 packets / 1 s.
    assert_eq!(reading.value, 1000);
    assert_eq!(reading.window, Duration64::from_secs(1));
}
