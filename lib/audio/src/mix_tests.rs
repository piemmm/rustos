//! Unit tests for the mixer.

// Exactness is the property under test in this module: a tolerance would
// accept precisely the imprecision the assertions exist to forbid.
#![allow(clippy::float_cmp)]

use alloc::vec;
use alloc::vec::Vec;

use super::{Mixer, SinkFormat, StreamMix, PIVOT_BITS};
use crate::channel::ChannelMatrix;
use crate::convert::Dither;
use tairix_abi::driver::audio::{ChannelMap, Rate, SampleFormat};
use tairix_abi::Errno;

fn sink(format: SampleFormat, map: ChannelMap) -> SinkFormat {
    SinkFormat {
        format,
        rate: Rate::HZ_48000,
        channel_map: map,
    }
}

fn identity(map: ChannelMap) -> ChannelMatrix {
    ChannelMatrix::derive(&map, &map).expect("a layout maps onto itself")
}

/// Interleaved sixteen-bit frames from raw sample values.
fn s16(values: &[i16]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// The headline property, at its simplest: one stream, unity gain, identity
/// map, the sink's own encoding, no other contributor.
#[test]
fn one_stream_at_unity_through_an_identity_map_is_bit_exact() {
    let map = ChannelMap::STEREO;
    let matrix = identity(map);
    let mut mixer = Mixer::new(sink(SampleFormat::S16, map), 8, 1).expect("mixer");
    let samples = s16(&[
        -32_768, 32_767, -1, 1, 0, -12_345, 23_456, -23_456, 1, -1, 4_660, -4_660, 32_767, -32_768,
        100, -100,
    ]);
    let mut out = vec![0u8; samples.len()];
    let stream = StreamMix {
        format: SampleFormat::S16,
        matrix: &matrix,
        gain: 1.0,
        resampled: false,
        samples: &samples,
    };
    let written = mixer.mix([stream], 8, &mut out).expect("mixed");
    assert_eq!(written, samples.len());
    assert_eq!(out, samples, "the one path must not touch a sample");
}

/// `0.0 + -0.0` is `+0.0`, so a zeroed accumulator would flatten a float
/// source's negative zero. The first contributor is assigned instead.
#[test]
fn a_lone_float_streams_negative_zero_survives_the_accumulator() {
    let map = ChannelMap::MONO;
    let matrix = identity(map);
    let mut mixer = Mixer::new(sink(SampleFormat::F32, map), 4, 1).expect("mixer");
    let samples: Vec<u8> = [-0.0f32, 0.0, -0.0, 0.5]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let mut out = vec![0u8; samples.len()];
    let stream = StreamMix {
        format: SampleFormat::F32,
        matrix: &matrix,
        gain: 1.0,
        resampled: false,
        samples: &samples,
    };
    mixer.mix([stream], 4, &mut out).expect("mixed");
    assert_eq!(out, samples);
}

#[test]
fn two_streams_sum() {
    let map = ChannelMap::MONO;
    let matrix = identity(map);
    let mut mixer = Mixer::new(sink(SampleFormat::S16, map), 4, 1).expect("mixer");
    mixer.set_dither(Dither::None);
    let first = s16(&[1_000, -2_000, 0, 500]);
    let second = s16(&[2_000, 1_000, 0, -500]);
    let mut out = vec![0u8; first.len()];
    let streams = [
        StreamMix {
            format: SampleFormat::S16,
            matrix: &matrix,
            gain: 1.0,
            resampled: false,
            samples: &first,
        },
        StreamMix {
            format: SampleFormat::S16,
            matrix: &matrix,
            gain: 1.0,
            resampled: false,
            samples: &second,
        },
    ];
    mixer
        .mix(streams.iter().copied(), 4, &mut out)
        .expect("mixed");
    assert_eq!(out, s16(&[3_000, -1_000, 0, 0]));
}

#[test]
fn the_sum_saturates_rather_than_wrapping() {
    let map = ChannelMap::MONO;
    let matrix = identity(map);
    let mut mixer = Mixer::new(sink(SampleFormat::S16, map), 2, 1).expect("mixer");
    mixer.set_dither(Dither::None);
    let loud = s16(&[30_000, -30_000]);
    let mut out = vec![0u8; loud.len()];
    let streams = [
        StreamMix {
            format: SampleFormat::S16,
            matrix: &matrix,
            gain: 1.0,
            resampled: false,
            samples: &loud,
        },
        StreamMix {
            format: SampleFormat::S16,
            matrix: &matrix,
            gain: 1.0,
            resampled: false,
            samples: &loud,
        },
    ];
    mixer
        .mix(streams.iter().copied(), 2, &mut out)
        .expect("mixed");
    assert_eq!(out, s16(&[32_767, -32_768]));
}

/// A tenant writing a `NaN` would otherwise silence every other stream on
/// the sink, which is a denial of service rather than a bad sample.
#[test]
fn one_streams_non_finite_samples_do_not_poison_another() {
    let map = ChannelMap::MONO;
    let matrix = identity(map);
    let mut mixer = Mixer::new(sink(SampleFormat::S16, map), 4, 1).expect("mixer");
    mixer.set_dither(Dither::None);
    let hostile: Vec<u8> = [f32::NAN, f32::INFINITY, f32::NAN, f32::NEG_INFINITY]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let honest = s16(&[1_000, 2_000, 3_000, 4_000]);
    let mut out = vec![0u8; honest.len()];
    let streams = [
        StreamMix {
            format: SampleFormat::F32,
            matrix: &matrix,
            gain: 1.0,
            resampled: false,
            samples: &hostile,
        },
        StreamMix {
            format: SampleFormat::S16,
            matrix: &matrix,
            gain: 1.0,
            resampled: false,
            samples: &honest,
        },
    ];
    mixer
        .mix(streams.iter().copied(), 4, &mut out)
        .expect("mixed");
    assert_eq!(out, honest, "the well-behaved stream was corrupted");
}

/// Zeroed bytes would be full negative deflection in the unsigned encoding,
/// which is a click rather than a gap.
#[test]
fn a_period_nothing_contributed_to_is_the_devices_own_silence() {
    for format in [SampleFormat::U8, SampleFormat::S16, SampleFormat::F32] {
        let map = ChannelMap::STEREO;
        let mut mixer = Mixer::new(sink(format, map), 4, 1).expect("mixer");
        let mut out = vec![0x5Au8; 4 * map.channels() as usize * format.bytes_per_sample()];
        let written = mixer.mix([], 4, &mut out).expect("mixed");
        assert_eq!(written, out.len());
        assert!(
            out.iter().all(|byte| *byte == format.silence_byte()),
            "{format:?} did not fill with its own quiet value"
        );
    }
}

#[test]
fn a_stream_shorter_than_the_period_leaves_the_rest_silent() {
    let map = ChannelMap::MONO;
    let matrix = identity(map);
    let mut mixer = Mixer::new(sink(SampleFormat::S16, map), 8, 1).expect("mixer");
    mixer.set_dither(Dither::None);
    let short = s16(&[1_000, 2_000]);
    let mut out = vec![0x5Au8; 16];
    mixer
        .mix(
            [StreamMix {
                format: SampleFormat::S16,
                matrix: &matrix,
                gain: 1.0,
                resampled: false,
                samples: &short,
            }],
            8,
            &mut out,
        )
        .expect("mixed");
    assert_eq!(&out[..4], &short[..]);
    assert!(
        out[4..].iter().all(|byte| *byte == 0),
        "the tail is not silent"
    );
}

#[test]
fn the_gain_is_applied_once() {
    let map = ChannelMap::MONO;
    let matrix = identity(map);
    let mut mixer = Mixer::new(sink(SampleFormat::S16, map), 2, 1).expect("mixer");
    mixer.set_dither(Dither::None);
    let samples = s16(&[1_000, -2_000]);
    let mut out = vec![0u8; samples.len()];
    mixer
        .mix(
            [StreamMix {
                format: SampleFormat::S16,
                matrix: &matrix,
                gain: 0.5,
                resampled: false,
                samples: &samples,
            }],
            2,
            &mut out,
        )
        .expect("mixed");
    assert_eq!(out, s16(&[500, -1_000]));
}

/// Adding noise to a path that was exact would destroy the property the
/// whole crate exists to keep.
#[test]
fn dither_is_not_applied_where_nothing_is_narrowed() {
    let map = ChannelMap::MONO;
    let matrix = identity(map);
    // Default dither on, but the source and the sink are the same width, so
    // the quantiser must still be bare.
    let mut mixer = Mixer::new(sink(SampleFormat::S16, map), 8, 99).expect("mixer");
    let samples = s16(&[1, 2, 3, 4, 5, 6, 7, 8]);
    let mut out = vec![0u8; samples.len()];
    mixer
        .mix(
            [StreamMix {
                format: SampleFormat::S16,
                matrix: &matrix,
                gain: 1.0,
                resampled: false,
                samples: &samples,
            }],
            8,
            &mut out,
        )
        .expect("mixed");
    assert_eq!(out, samples);
}

#[test]
fn dither_is_applied_where_the_destination_is_narrower() {
    let map = ChannelMap::MONO;
    let matrix = identity(map);
    // A twenty-four-bit source into a sixteen-bit sink: constant material,
    // so every output frame would be identical without dither.
    let source: Vec<u8> = (0..64).flat_map(|_| [0x00u8, 0x40, 0x00]).collect();
    let mut dithered = vec![0u8; 128];
    let mut bare = vec![0u8; 128];
    let stream = StreamMix {
        format: SampleFormat::S24,
        matrix: &matrix,
        gain: 1.0,
        resampled: false,
        samples: &source,
    };
    Mixer::new(sink(SampleFormat::S16, map), 64, 7)
        .expect("mixer")
        .mix([stream], 64, &mut dithered)
        .expect("mixed");
    let mut plain = Mixer::new(sink(SampleFormat::S16, map), 64, 7).expect("mixer");
    plain.set_dither(Dither::None);
    plain.mix([stream], 64, &mut bare).expect("mixed");
    assert_ne!(dithered, bare, "the narrowing path was not dithered");
    // And the dither moved each sample by at most one step.
    for frame in 0..64 {
        let with = i16::from_le_bytes([dithered[frame * 2], dithered[frame * 2 + 1]]);
        let without = i16::from_le_bytes([bare[frame * 2], bare[frame * 2 + 1]]);
        assert!((with - without).abs() <= 1, "frame {frame}");
    }
}

/// A sum carries content below every contributor's own step, so two
/// sixteen-bit streams into a sixteen-bit sink is still a narrowing.
#[test]
fn a_sum_is_treated_as_carrying_the_pivots_resolution() {
    assert_eq!(PIVOT_BITS, 24);
    let map = ChannelMap::MONO;
    let matrix = identity(map);
    let quiet = s16(&[0; 64]);
    let stream = StreamMix {
        format: SampleFormat::S16,
        matrix: &matrix,
        gain: 0.5,
        resampled: false,
        samples: &quiet,
    };
    let mut out = vec![0u8; 128];
    let mut mixer = Mixer::new(sink(SampleFormat::S16, map), 64, 3).expect("mixer");
    mixer.mix([stream, stream], 64, &mut out).expect("mixed");
    // Silence plus silence plus one step of dither is not exactly silence.
    assert!(
        out.iter().any(|byte| *byte != 0),
        "two contributors into a sixteen-bit sink must dither"
    );
}

#[test]
fn a_matrix_derived_against_a_different_sink_is_refused() {
    let matrix = identity(ChannelMap::MONO);
    let mut mixer = Mixer::new(sink(SampleFormat::S16, ChannelMap::STEREO), 4, 1).expect("mixer");
    let samples = s16(&[0, 0, 0, 0]);
    let mut out = vec![0u8; 16];
    assert_eq!(
        mixer.mix(
            [StreamMix {
                format: SampleFormat::S16,
                matrix: &matrix,
                gain: 1.0,
                resampled: false,
                samples: &samples,
            }],
            4,
            &mut out
        ),
        Err(Errno::NotSupported)
    );
}

#[test]
fn a_period_past_the_configured_one_or_a_short_destination_is_refused() {
    let map = ChannelMap::STEREO;
    let mut mixer = Mixer::new(sink(SampleFormat::S16, map), 4, 1).expect("mixer");
    let mut out = vec![0u8; 16];
    assert_eq!(mixer.mix([], 5, &mut out), Err(Errno::OutOfRange));
    let mut small = vec![0u8; 4];
    assert_eq!(mixer.mix([], 4, &mut small), Err(Errno::BufferTooSmall));
}

#[test]
fn a_zero_period_is_refused_at_construction() {
    assert_eq!(
        Mixer::new(sink(SampleFormat::S16, ChannelMap::STEREO), 0, 1).err(),
        Some(Errno::OutOfRange)
    );
}

#[test]
fn a_downmixed_stream_lands_in_the_sinks_layout() {
    let matrix = ChannelMatrix::derive(&ChannelMap::STEREO, &ChannelMap::MONO).expect("fold");
    let mut mixer = Mixer::new(sink(SampleFormat::S16, ChannelMap::MONO), 2, 1).expect("mixer");
    mixer.set_dither(Dither::None);
    let stereo = s16(&[1_000, 3_000, -2_000, 0]);
    let mut out = vec![0u8; 4];
    mixer
        .mix(
            [StreamMix {
                format: SampleFormat::S16,
                matrix: &matrix,
                gain: 1.0,
                resampled: false,
                samples: &stereo,
            }],
            2,
            &mut out,
        )
        .expect("mixed");
    assert_eq!(out, s16(&[2_000, -1_000]));
}
