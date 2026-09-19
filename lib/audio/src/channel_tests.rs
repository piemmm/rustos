//! Unit tests for the channel matrix.
//!
//! The downmix coefficients are checked against ITU-R BS.775's own numbers
//! rather than against whatever the table happens to hold, so a transcription
//! slip shows up here rather than as a surround mix that sounds slightly
//! wrong.

// Exactness is the property under test in this module: a tolerance would
// accept precisely the imprecision the assertions exist to forbid.
#![allow(clippy::float_cmp)]

use alloc::vec;

use super::{ChannelMatrix, FOLD};
use tairix_abi::driver::audio::{ChannelMap, ChannelPosition};
use tairix_abi::Errno;

/// The 5.1 layout in its conventional interleave order.
fn surround_51() -> ChannelMap {
    ChannelMap::new(&[
        ChannelPosition::FrontLeft,
        ChannelPosition::FrontRight,
        ChannelPosition::FrontCentre,
        ChannelPosition::LowFrequency,
        ChannelPosition::RearLeft,
        ChannelPosition::RearRight,
    ])
    .expect("a valid 5.1 layout")
}

/// The 7.1 layout: 5.1 with the side pair.
fn surround_71() -> ChannelMap {
    ChannelMap::new(&[
        ChannelPosition::FrontLeft,
        ChannelPosition::FrontRight,
        ChannelPosition::FrontCentre,
        ChannelPosition::LowFrequency,
        ChannelPosition::RearLeft,
        ChannelPosition::RearRight,
        ChannelPosition::SideLeft,
        ChannelPosition::SideRight,
    ])
    .expect("a valid 7.1 layout")
}

#[track_caller]
fn close(got: f32, want: f32) {
    assert!((got - want).abs() < 1e-6, "coefficient {got} is not {want}");
}

#[test]
fn equal_layouts_are_the_identity() {
    for map in [
        ChannelMap::MONO,
        ChannelMap::STEREO,
        surround_51(),
        surround_71(),
    ] {
        let matrix = ChannelMatrix::derive(&map, &map).expect("a layout maps onto itself");
        assert!(matrix.is_identity(), "{map:?} is not its own identity");
        for destination in 0..matrix.destinations() {
            for source in 0..matrix.sources() {
                let want = if destination == source { 1.0 } else { 0.0 };
                close(matrix.weight(destination, source), want);
            }
        }
    }
}

/// Bit-exactness needs the identity path to be a copy, including the one
/// value a multiply-accumulate turns into its positive twin.
#[test]
fn the_identity_at_unity_gain_copies_negative_zero_through() {
    let matrix = ChannelMatrix::derive(&ChannelMap::STEREO, &ChannelMap::STEREO).expect("stereo");
    let source = [-0.0f32, 0.25, 0.0, -0.5];
    let mut out = vec![9.0f32; 4];
    matrix.map(2, 1.0, &source, &mut out).expect("sized");
    assert_eq!(out, source.to_vec());
    assert!(out[0].is_sign_negative(), "negative zero was flattened");
}

#[test]
fn a_monophonic_source_reaches_both_fronts_at_unity() {
    let matrix =
        ChannelMatrix::derive(&ChannelMap::MONO, &ChannelMap::STEREO).expect("mono into stereo");
    assert!(!matrix.is_identity());
    close(matrix.weight(0, 0), 1.0);
    close(matrix.weight(1, 0), 1.0);
    let mut out = vec![0.0f32; 4];
    matrix.map(2, 1.0, &[0.5, -0.25], &mut out).expect("sized");
    assert_eq!(out, vec![0.5, 0.5, -0.25, -0.25]);
}

#[test]
fn a_stereo_source_folds_into_one_channel_at_half_each() {
    let matrix =
        ChannelMatrix::derive(&ChannelMap::STEREO, &ChannelMap::MONO).expect("stereo into mono");
    close(matrix.weight(0, 0), 0.5);
    close(matrix.weight(0, 1), 0.5);
    let mut out = vec![0.0f32; 2];
    matrix
        .map(2, 1.0, &[1.0, 0.0, 0.5, 0.5], &mut out)
        .expect("sized");
    assert_eq!(out, vec![0.5, 0.5]);
}

/// The recommendation's own coefficients: centre and each surround enter both
/// fronts at one over root two, and the fronts pass straight through.
#[test]
fn five_point_one_folds_to_stereo_on_the_bs775_coefficients() {
    let matrix = ChannelMatrix::derive(&surround_51(), &ChannelMap::STEREO).expect("5.1 to stereo");
    // Left output.
    close(matrix.weight(0, 0), 1.0);
    close(matrix.weight(0, 1), 0.0);
    close(matrix.weight(0, 2), FOLD);
    close(matrix.weight(0, 4), FOLD);
    close(matrix.weight(0, 5), 0.0);
    // Right output.
    close(matrix.weight(1, 1), 1.0);
    close(matrix.weight(1, 2), FOLD);
    close(matrix.weight(1, 5), FOLD);
    close(matrix.weight(1, 4), 0.0);
}

/// The low-frequency channel is excluded from the downmix by the
/// recommendation, so it must reach nothing rather than be folded in.
#[test]
fn the_low_frequency_channel_is_dropped_by_rule_when_the_sink_has_none() {
    let matrix = ChannelMatrix::derive(&surround_51(), &ChannelMap::STEREO).expect("5.1 to stereo");
    close(matrix.weight(0, 3), 0.0);
    close(matrix.weight(1, 3), 0.0);
    // And it survives untouched where the sink does carry one.
    let same = ChannelMatrix::derive(&surround_51(), &surround_51()).expect("identity");
    close(same.weight(3, 3), 1.0);
}

#[test]
fn seven_point_one_prefers_the_rear_slots_before_folding_to_the_fronts() {
    let to_51 = ChannelMatrix::derive(&surround_71(), &surround_51()).expect("7.1 into 5.1");
    // Side left has no slot of its own in 5.1, so it takes the rear's.
    close(to_51.weight(4, 6), 1.0);
    close(to_51.weight(5, 7), 1.0);
    // And the rears keep theirs, so both land in the same destination.
    close(to_51.weight(4, 4), 1.0);

    let to_stereo = ChannelMatrix::derive(&surround_71(), &ChannelMap::STEREO).expect("7.1 down");
    close(to_stereo.weight(0, 6), FOLD);
    close(to_stereo.weight(1, 7), FOLD);
}

#[test]
fn an_upmix_leaves_the_channels_the_source_does_not_carry_silent() {
    let matrix = ChannelMatrix::derive(&ChannelMap::STEREO, &surround_51()).expect("stereo up");
    close(matrix.weight(0, 0), 1.0);
    close(matrix.weight(1, 1), 1.0);
    for destination in 2..6 {
        for source in 0..2 {
            close(matrix.weight(destination, source), 0.0);
        }
    }
    let mut out = vec![9.0f32; 6];
    matrix.map(1, 1.0, &[0.75, -0.75], &mut out).expect("sized");
    assert_eq!(out, vec![0.75, -0.75, 0.0, 0.0, 0.0, 0.0]);
}

/// Copying a channel somewhere arbitrary would be a silent corruption of the
/// material, so a pair with no defined relationship is refused instead.
#[test]
fn a_layout_pair_with_no_defined_relationship_is_refused() {
    let only_low = ChannelMap::new(&[ChannelPosition::LowFrequency]).expect("valid");
    assert_eq!(
        ChannelMatrix::derive(&ChannelMap::STEREO, &only_low),
        Err(Errno::NotSupported)
    );
    let only_side =
        ChannelMap::new(&[ChannelPosition::SideLeft, ChannelPosition::SideRight]).expect("valid");
    // Sides fold onto the fronts, which this sink does not have either.
    assert_eq!(
        ChannelMatrix::derive(&only_side, &only_low),
        Err(Errno::NotSupported)
    );
}

#[test]
fn the_gain_is_applied_once_across_the_whole_map() {
    let matrix = ChannelMatrix::derive(&ChannelMap::MONO, &ChannelMap::STEREO).expect("up");
    let mut out = vec![0.0f32; 2];
    matrix.map(1, 0.5, &[1.0], &mut out).expect("sized");
    assert_eq!(out, vec![0.5, 0.5]);
}

#[test]
fn a_short_buffer_on_either_side_is_refused() {
    let matrix = ChannelMatrix::derive(&ChannelMap::STEREO, &ChannelMap::STEREO).expect("stereo");
    let mut out = vec![0.0f32; 2];
    assert_eq!(
        matrix.map(2, 1.0, &[0.0, 0.0], &mut out),
        Err(Errno::BufferTooSmall)
    );
    let mut small = vec![0.0f32; 1];
    assert_eq!(
        matrix.map(1, 1.0, &[0.0, 0.0], &mut small),
        Err(Errno::BufferTooSmall)
    );
}

/// A fold must not manufacture level: the recommendation's coefficients keep
/// a full-scale surround mix inside what the front pair can carry once the
/// quantiser saturates, and the matrix must not be quietly louder than that.
#[test]
fn the_downmix_coefficients_are_the_published_ones_and_nothing_larger() {
    let matrix = ChannelMatrix::derive(&surround_71(), &ChannelMap::STEREO).expect("7.1 down");
    for destination in 0..matrix.destinations() {
        for source in 0..matrix.sources() {
            let weight = matrix.weight(destination, source);
            assert!(
                (0.0..=1.0).contains(&weight),
                "coefficient ({destination}, {source}) is {weight}"
            );
        }
    }
}
