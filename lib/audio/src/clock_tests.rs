//! Unit tests for the device clock model.
//!
//! The rate estimate is checked against a **synthesised** device whose real
//! rate is deliberately not its nominal one, because getting the nominal rate
//! back is exactly the failure this model exists to avoid.

use super::{ClockModel, MIN_SAMPLES, PLAUSIBLE_PPM, WINDOW};
use tairix_abi::audio::decode_clock_reply;
use tairix_abi::audio::encode_clock_reply;
use tairix_abi::driver::audio::{Frames, Rate};
use tairix_abi::time::Time64;
use tairix_abi::Errno;

fn rate(hz: u32) -> Rate {
    Rate::new(hz).expect("a rate inside the vocabulary")
}

/// Whole nanoseconds since the epoch, so a test's own arithmetic cannot
/// quietly drop a sub-second part the model kept.
fn nanos_of(time: Time64) -> i128 {
    i128::from(time.secs()) * 1_000_000_000 + i128::from(time.subsec_nanos())
}

fn at(nanos: i128) -> Time64 {
    let secs = i64::try_from(nanos.div_euclid(1_000_000_000)).expect("in range");
    let sub = u32::try_from(nanos.rem_euclid(1_000_000_000)).expect("in range");
    Time64::new(secs, sub).expect("a canonical time")
}

/// The whole frame count a device running at `rate` frames per second has
/// reached after `nanos` nanoseconds.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a synthesised span of a few hundred milliseconds at an audio \
              rate, where a double is exact and the product is far inside u64"
)]
fn position_of(rate: f64, nanos: i128) -> u64 {
    (rate * (nanos as f64) / 1e9) as u64
}

/// Feed `periods` reports from a device whose real rate is `actual` frames
/// per second while its crystal claims `nominal`.
fn run(nominal: u32, actual: f64, periods: usize) -> ClockModel {
    let mut model = ClockModel::new(rate(nominal));
    // A five-millisecond period, which is what a low-latency sink runs at.
    let period_nanos = 5_000_000i128;
    for step in 0..periods {
        let elapsed = period_nanos * i128::try_from(step).expect("small");
        model
            .observe(Frames::new(position_of(actual, elapsed)), at(elapsed))
            .expect("a well-behaved device");
    }
    model
}

#[test]
fn the_measured_rate_is_the_devices_real_one_not_its_label() {
    let model = run(48_000, 47_998.6, WINDOW);
    let measured = model
        .measured_millihertz()
        .expect("a full window of clean reports is measurable");
    let error = i64::from(measured) - 47_998_600;
    assert!(
        error.abs() < 2_000,
        "measured {measured} millihertz against a real 47 998 600"
    );
    assert_ne!(measured, model.nominal_millihertz());
}

#[test]
fn a_short_window_says_it_does_not_know_and_the_nominal_rate_stands() {
    let model = run(48_000, 47_998.6, MIN_SAMPLES - 1);
    assert_eq!(model.measured_millihertz(), None);
    assert_eq!(model.reported_millihertz(), 48_000_000);
}

#[test]
fn nothing_observed_yet_has_no_report_at_all() {
    let model = ClockModel::new(rate(48_000));
    assert_eq!(model.report(), None);
    assert_eq!(model.latest(), None);
    assert_eq!(model.position_at(at(0)), None);
    assert_eq!(model.time_at(Frames::new(100)), None);
}

/// A broken or hostile driver degrades its own device's reported rate to the
/// nominal one and poisons nothing.
#[test]
fn a_position_or_a_time_that_went_backwards_is_refused() {
    let mut model = ClockModel::new(rate(48_000));
    model
        .observe(Frames::new(4_800), at(100_000_000))
        .expect("first");
    assert_eq!(
        model.observe(Frames::new(4_799), at(200_000_000)),
        Err(Errno::OutOfRange)
    );
    assert_eq!(
        model.observe(Frames::new(9_600), at(50_000_000)),
        Err(Errno::OutOfRange)
    );
    // The accepted pair is still the one it was.
    assert_eq!(
        model.latest().map(|(position, _)| position),
        Some(Frames::new(4_800))
    );
}

#[test]
fn a_pair_implying_an_impossible_rate_is_refused() {
    let mut model = ClockModel::new(rate(48_000));
    model.observe(Frames::ZERO, at(0)).expect("first");
    // A hundred million frames in a millisecond is not a converter.
    assert_eq!(
        model.observe(Frames::new(100_000_000), at(1_000_000)),
        Err(Errno::OutOfRange)
    );
}

/// A crystal is accurate to parts per million; a fit that says otherwise is
/// measuring something other than the device's rate.
#[test]
fn an_implausible_fit_is_not_believed() {
    let model = run(48_000, 24_000.0, WINDOW);
    assert_eq!(model.measured_millihertz(), None);
    assert_eq!(model.reported_millihertz(), 48_000_000);
    // And a drift just inside the band is.
    let inside = f64::from(48_000u32)
        * (1.0 - f64::from(u32::try_from(PLAUSIBLE_PPM).expect("small")) / 4e6);
    let believed = run(48_000, inside, WINDOW);
    assert!(believed.measured_millihertz().is_some());
}

#[test]
fn the_window_holds_only_its_most_recent_observations() {
    // Run well past the window, then check the fit tracks the *recent* rate
    // rather than an average over everything ever seen.
    let mut model = ClockModel::new(rate(48_000));
    let mut nanos = 0i128;
    let mut position = 0u64;
    for step in 0..WINDOW * 4 {
        // The first half runs slow, the second half fast; only the second is
        // still in the window at the end.
        let rate_now = if step < WINDOW * 2 {
            47_900.0
        } else {
            48_090.0
        };
        model
            .observe(Frames::new(position), at(nanos))
            .expect("well-behaved");
        nanos += 5_000_000;
        position += position_of(rate_now, 5_000_000);
    }
    let measured = model.measured_millihertz().expect("measurable");
    assert!(
        (i64::from(measured) - 48_090_000).abs() < 200_000,
        "the window kept stale observations: measured {measured}"
    );
}

#[test]
fn the_exported_map_runs_both_ways_and_agrees_with_itself() {
    let model = run(48_000, 48_000.0, WINDOW);
    let (position, sampled_at) = model.latest().expect("observed");
    // A second later the device has advanced by its measured rate.
    let later = at(nanos_of(sampled_at) + 1_000_000_000);
    let ahead = model.position_at(later).expect("mappable");
    assert!(
        ahead.get().abs_diff(position.get() + 48_000) < 50,
        "a second ahead landed at {ahead:?} from {position:?}"
    );
    // And back again.
    let when = model.time_at(ahead).expect("mappable");
    let drift = nanos_of(when) - nanos_of(later);
    assert!(drift.abs() < 2_000_000, "the map disagreed by {drift} ns");
}

#[test]
fn a_reset_forgets_a_clock_that_is_no_longer_running() {
    let mut model = run(48_000, 47_998.6, WINDOW);
    assert!(model.measured_millihertz().is_some());
    model.reset();
    assert_eq!(model.measured_millihertz(), None);
    assert_eq!(model.report(), None);
    // And a position from the old clock is accepted again rather than
    // refused as going backwards.
    model.observe(Frames::ZERO, at(0)).expect("a fresh clock");
}

/// Whatever the fit produces has to survive the wire, or a measured rate
/// would be a report the client refuses to decode.
#[test]
fn every_reported_rate_round_trips_through_the_protocol() {
    for (nominal, actual) in [
        (48_000u32, 47_998.6f64),
        (48_000, 48_001.2),
        (44_100, 44_100.0),
        (8_000, 8_000.0),
        (192_000, 192_000.0),
    ] {
        let model = run(nominal, actual, WINDOW);
        let report = model.report().expect("observed");
        let frame = encode_clock_reply(Ok(report));
        let back = decode_clock_reply(&frame).expect("a reported rate must be decodable");
        assert_eq!(back, report, "at {actual}");
    }
}
