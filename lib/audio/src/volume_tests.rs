//! Unit tests for the volume model.

// Exactness is the property under test in this module: a tolerance would
// accept precisely the imprecision the assertions exist to forbid.
#![allow(clippy::float_cmp)]

use super::{
    fraction_to_millibel, millibel_to_linear, resolve, VolumeRequest, DEFAULT_FLOOR_MILLIBEL,
    DUCK_MILLIBEL, UNITY_MILLIBEL,
};
use tairix_abi::driver::audio::GainRange;

/// A conventional codec control: sixty-four decibels of attenuation in half-
/// decibel steps, reaching unity at the top.
fn codec_range() -> GainRange {
    GainRange::new(-6_400, 0, 50).expect("a valid range")
}

#[track_caller]
fn close(got: f32, want: f32) {
    assert!((got - want).abs() < 1e-4, "{got} is not {want}");
}

/// Not merely close to one. The stack's bit-exactness claim rests on this
/// multiply changing nothing at all.
#[test]
fn unity_is_exactly_one() {
    assert_eq!(millibel_to_linear(UNITY_MILLIBEL), 1.0);
    let sample = 0.123_456_79f32;
    assert_eq!(sample * millibel_to_linear(0), sample);
}

#[test]
fn the_decibel_curve_matches_its_definition() {
    close(millibel_to_linear(-600), 0.501_187_2);
    close(millibel_to_linear(-2_000), 0.1);
    close(millibel_to_linear(-4_000), 0.01);
    close(millibel_to_linear(600), 1.995_262_3);
}

#[test]
fn the_curve_is_monotone_and_saturates_instead_of_overflowing() {
    let mut previous = 0.0f32;
    let mut millibel = -20_000;
    while millibel <= 2_000 {
        let gain = millibel_to_linear(millibel);
        assert!(gain.is_finite() && gain >= previous, "at {millibel}");
        previous = gain;
        millibel += 137;
    }
    assert!(millibel_to_linear(i32::MIN) >= 0.0);
    assert!(millibel_to_linear(i32::MAX).is_finite());
}

#[test]
fn the_taper_runs_from_the_floor_to_unity_and_clamps_outside() {
    assert_eq!(fraction_to_millibel(1.0, DEFAULT_FLOOR_MILLIBEL), 0);
    assert_eq!(
        fraction_to_millibel(0.0, DEFAULT_FLOOR_MILLIBEL),
        DEFAULT_FLOOR_MILLIBEL
    );
    // Linear in decibels: half the travel is half the attenuation.
    assert_eq!(
        fraction_to_millibel(0.5, DEFAULT_FLOOR_MILLIBEL),
        DEFAULT_FLOOR_MILLIBEL / 2
    );
    assert_eq!(
        fraction_to_millibel(-1.0, DEFAULT_FLOOR_MILLIBEL),
        DEFAULT_FLOOR_MILLIBEL
    );
    assert_eq!(fraction_to_millibel(9.0, DEFAULT_FLOOR_MILLIBEL), 0);
    assert_eq!(
        fraction_to_millibel(f32::NAN, DEFAULT_FLOOR_MILLIBEL),
        DEFAULT_FLOOR_MILLIBEL
    );
}

#[test]
fn the_four_gains_sum_and_saturate() {
    let request = VolumeRequest {
        stream_millibel: -100,
        application_millibel: -200,
        sink_millibel: -300,
        duck_millibel: DUCK_MILLIBEL,
        muted: false,
    };
    assert_eq!(request.total_millibel(), -600 + DUCK_MILLIBEL);
    let absurd = VolumeRequest {
        stream_millibel: i32::MIN,
        application_millibel: i32::MIN,
        ..VolumeRequest::default()
    };
    assert_eq!(absurd.total_millibel(), i32::MIN);
}

/// The point of using the device's own control: where it can deliver the
/// whole gain, the mixer multiplies by exactly one and the path stays exact.
#[test]
fn a_gain_on_the_devices_own_grid_leaves_the_software_multiply_at_unity() {
    for millibel in [0, -50, -1_000, -6_400] {
        let request = VolumeRequest {
            stream_millibel: millibel,
            ..VolumeRequest::default()
        };
        let resolved = resolve(&request, Some(codec_range()));
        assert_eq!(resolved.hardware_millibel, Some(millibel));
        assert_eq!(resolved.software, 1.0, "at {millibel}");
        assert_eq!(resolved.total_millibel, millibel);
    }
}

/// Rounding the hardware setting the other way would leave software making
/// the difference up with gain, on a path with no headroom to spare.
#[test]
fn an_off_grid_gain_leaves_the_software_remainder_as_attenuation() {
    let request = VolumeRequest {
        stream_millibel: -1_025,
        ..VolumeRequest::default()
    };
    let resolved = resolve(&request, Some(codec_range()));
    assert_eq!(resolved.hardware_millibel, Some(-1_000));
    assert!(
        resolved.software < 1.0,
        "software must attenuate, not amplify: {}",
        resolved.software
    );
    close(resolved.software, millibel_to_linear(-25));
}

#[test]
fn a_gain_below_what_the_hardware_reaches_is_finished_in_software() {
    let request = VolumeRequest {
        stream_millibel: -9_000,
        ..VolumeRequest::default()
    };
    let resolved = resolve(&request, Some(codec_range()));
    assert_eq!(resolved.hardware_millibel, Some(-6_400));
    close(resolved.software, millibel_to_linear(-2_600));
    assert_eq!(resolved.total_millibel, -9_000);
}

#[test]
fn a_device_with_no_control_takes_the_whole_gain_in_software() {
    let request = VolumeRequest {
        stream_millibel: -1_234,
        ..VolumeRequest::default()
    };
    let resolved = resolve(&request, None);
    assert_eq!(resolved.hardware_millibel, None);
    close(resolved.software, millibel_to_linear(-1_234));
}

#[test]
fn mute_is_silence_in_both_halves_and_keeps_the_level_it_would_return_to() {
    let request = VolumeRequest {
        stream_millibel: -500,
        muted: true,
        ..VolumeRequest::default()
    };
    let resolved = resolve(&request, Some(codec_range()));
    assert_eq!(resolved.software, 0.0);
    assert_eq!(resolved.hardware_millibel, Some(-6_400));
    assert!(resolved.muted);
    // The reported total is still the level, so unmuting restores it.
    assert_eq!(resolved.total_millibel, -500);
}

#[test]
fn a_grid_that_does_not_reach_the_top_settles_on_the_loudest_setting() {
    // A step that does not divide the range: the grid's last point is short
    // of the maximum, so the maximum itself is the closest the device has.
    let awkward = GainRange::new(-1_000, 0, 300).expect("valid");
    let request = VolumeRequest::default();
    let resolved = resolve(&request, Some(awkward));
    let setting = resolved.hardware_millibel.expect("a control is present");
    assert!((-1_000..=0).contains(&setting), "{setting}");
    assert!(
        resolved.software <= 1.0,
        "software amplified to make up the shortfall"
    );
}

#[test]
fn the_duck_step_is_a_real_attenuation_and_not_silence() {
    let ducked = millibel_to_linear(DUCK_MILLIBEL);
    assert!(
        ducked > 0.0 && ducked < 0.2,
        "ducking must leave the media audible but plainly under the speech: {ducked}"
    );
}
