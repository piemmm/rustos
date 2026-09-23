//! The stack's headline property, driven through the **whole** engine.
//!
//! > A source of twenty-four bits or fewer, at unity gain, at a rate and
//! > channel map the device accepts, with no other stream live, reaches the
//! > device bit-exact.
//!
//! This is what makes an exclusive or bypass mode unnecessary in TAIRiX: the
//! thing such a mode exists to escape does not happen on the one path. So it
//! is checked as a property over the cross-product of encodings, rates,
//! channel layouts and block lengths rather than as one example, and every
//! stage a real stream passes through is in the loop — the resampler at its
//! unity ratio, the derived channel matrix, the resolved volume, and the
//! mixer's quantisation to the device's own encoding.
//!
//! Two exclusions are deliberate and both are the crate's documented
//! position rather than a gap:
//!
//! * a thirty-two-bit **integer** source carries twenty-four bits of mantissa
//!   through the `f32` pivot, so it is checked for its bounded error instead;
//! * a rate the device cannot meet is resampled, and a resampled stream is
//!   not claimed to be bit-exact — the grant tells a client what it got, and
//!   the client owns the difference.

// Exactness is the property under test: a tolerance would accept precisely
// the imprecision these assertions exist to forbid.
#![allow(clippy::float_cmp)]

use tairix_abi::driver::audio::{ChannelMap, ChannelPosition, Rate, SampleFormat};
use tairix_audio::channel::ChannelMatrix;
use tairix_audio::convert::Dither;
use tairix_audio::mix::{Mixer, SinkFormat, StreamMix};
use tairix_audio::resample::{FilterBank, Resampler};
use tairix_audio::volume::{millibel_to_linear, resolve, VolumeRequest};
use tairix_fuzzseed::Prng;

/// Every encoding the pivot carries whole, which is the set the claim covers.
const EXACT_FORMATS: &[SampleFormat] = &[
    SampleFormat::U8,
    SampleFormat::S16,
    SampleFormat::S24,
    SampleFormat::S24In32,
    SampleFormat::F32,
];

/// Rates from across the standard family, so the property is not an accident
/// of one clock.
const RATES: &[u32] = &[8_000, 44_100, 48_000, 96_000, 192_000];

/// Block lengths chosen to straddle every boundary a buffer has: one frame,
/// a prime, a power of two, and one either side of it.
const BLOCKS: &[usize] = &[1, 2, 3, 7, 15, 16, 17, 64, 127, 128];

fn rate(hz: u32) -> Rate {
    Rate::new(hz).expect("a rate inside the vocabulary")
}

/// The channel layouts the sweep covers, from one channel to 7.1.
fn layouts() -> [ChannelMap; 5] {
    [
        ChannelMap::MONO,
        ChannelMap::STEREO,
        ChannelMap::new(&[
            ChannelPosition::FrontLeft,
            ChannelPosition::FrontRight,
            ChannelPosition::FrontCentre,
        ])
        .expect("a valid three-channel layout"),
        ChannelMap::new(&[
            ChannelPosition::FrontLeft,
            ChannelPosition::FrontRight,
            ChannelPosition::FrontCentre,
            ChannelPosition::LowFrequency,
            ChannelPosition::RearLeft,
            ChannelPosition::RearRight,
        ])
        .expect("a valid 5.1 layout"),
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
        .expect("a valid 7.1 layout"),
    ]
}

/// `count` pseudo-random but **valid** samples of `format`.
///
/// Valid matters: a `NaN` or an out-of-range word is not a sample of its
/// encoding, and the engine deliberately bounds both before they reach a
/// shared mix. Feeding one here would test that boundary rather than the
/// exactness claim, which is about real audio.
fn probes(format: SampleFormat, count: usize, rng: &mut Prng) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(count * format.bytes_per_sample());
    for _ in 0..count {
        let draw = rng.next_u32();
        let [b0, b1, ..] = draw.to_le_bytes();
        match format {
            SampleFormat::U8 => bytes.push(b0),
            SampleFormat::S16 => bytes.extend_from_slice(&[b0, b1]),
            SampleFormat::S24 | SampleFormat::S24In32 => {
                // Sign-extended twenty-four bits, which is what both packed
                // and containered forms of this encoding hold.
                let raw = i32::try_from(draw & 0x00FF_FFFF).unwrap_or(0);
                let value = if raw >= 0x0080_0000 {
                    raw - 0x0100_0000
                } else {
                    raw
                };
                if format == SampleFormat::S24 {
                    bytes.extend_from_slice(&value.to_le_bytes()[..3]);
                } else {
                    bytes.extend_from_slice(&value.to_le_bytes());
                }
            }
            SampleFormat::S32 => bytes.extend_from_slice(&draw.to_le_bytes()),
            SampleFormat::F32 => {
                // Inside full scale, where the pivot is the identity.
                let unit = f32::from(u16::from_le_bytes([b0, b1])) / 32_768.0 - 1.0;
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
        }
    }
    bytes
}

/// Run one block through every stage a real playback stream passes through,
/// and answer the bytes the device would be handed.
fn through_the_engine(
    format: SampleFormat,
    hz: u32,
    map: ChannelMap,
    frames: usize,
    samples: &[u8],
) -> Vec<u8> {
    let channels = usize::from(map.channels());

    // The device runs at the stream's own rate, so the one resampler in the
    // system is at its unity ratio and must be a copy.
    let bank = FilterBank::new(rate(hz), rate(hz)).expect("a unity bank");
    let mut resampler = Resampler::new(&bank, channels).expect("a resampler");
    assert!(bank.is_unity(), "the ratio should need no filtering");

    // The volume model at unity: exactly one, or the claim fails here.
    let level = resolve(&VolumeRequest::default(), None);
    assert_eq!(level.software, 1.0, "unity must be exactly one");
    assert_eq!(millibel_to_linear(0), 1.0);

    // The device carries the stream's own layout, so the matrix is the
    // identity and its map is a copy.
    let matrix = ChannelMatrix::derive(&map, &map).expect("a layout maps onto itself");
    assert!(matrix.is_identity());

    // Drive the samples through the resampler as a real stream would, then
    // re-encode them for the mixer, so the unity path is genuinely exercised
    // rather than skipped.
    let mut pivot = vec![0.0f32; frames * channels];
    let decoded = tairix_audio::convert::decode(format, samples, &mut pivot);
    assert_eq!(decoded, frames * channels);
    let mut filtered = vec![0.0f32; frames * channels];
    let (consumed, produced) = resampler
        .process(&bank, &pivot, &mut filtered)
        .expect("whole frames");
    assert_eq!((consumed, produced), (frames, frames));
    assert_eq!(filtered, pivot, "the unity ratio must not filter");
    let mut staged = vec![0u8; samples.len()];
    tairix_audio::convert::encode(format, &filtered, &mut staged);

    let mut mixer = Mixer::new(
        SinkFormat {
            format,
            rate: rate(hz),
            channel_map: map,
        },
        frames.max(1),
        0x5EED,
    )
    .expect("a mixer");
    // Dither left at its default: the narrowing test must decide on its own
    // that this path narrows nothing, rather than the test turning it off.
    let mut out = vec![0u8; samples.len()];
    let stream = StreamMix {
        format,
        matrix: &matrix,
        gain: level.software,
        resampled: false,
        samples: &staged,
    };
    let written = mixer.mix([stream], frames, &mut out).expect("mixed");
    assert_eq!(written, out.len());
    out
}

#[test]
fn the_engine_is_bit_exact_across_formats_rates_channel_counts_and_blocks() {
    let mut rng = Prng::new(1);
    for format in EXACT_FORMATS {
        for hz in RATES {
            for map in layouts() {
                let channels = usize::from(map.channels());
                for frames in BLOCKS {
                    let samples = probes(*format, frames * channels, &mut rng);
                    let out = through_the_engine(*format, *hz, map, *frames, &samples);
                    assert_eq!(
                        out, samples,
                        "{format:?} at {hz} Hz over {channels} channels, {frames} frames"
                    );
                }
            }
        }
    }
}

/// The mixer alone, sized for more frames than it is handed, so a partial
/// block is held to the same exactness as a full one.
#[test]
fn the_mixer_is_bit_exact_for_a_block_shorter_than_its_capacity() {
    let mut rng = Prng::new(0x243F_6A88_85A3_08D3);
    for map in &layouts()[..3] {
        let matrix = ChannelMatrix::derive(map, map).expect("a layout maps onto itself");
        let channels = usize::from(map.channels());
        for format in EXACT_FORMATS {
            for frames in [1usize, 2, 3, 7, 16, 31] {
                let sink = SinkFormat {
                    format: *format,
                    rate: rate(48_000),
                    channel_map: *map,
                };
                let mut mixer = Mixer::new(sink, 32, 0x00C0_FFEE).expect("a mixer");
                let samples = probes(*format, frames * channels, &mut rng);
                let mut out = vec![0u8; samples.len()];
                let stream = StreamMix {
                    format: *format,
                    matrix: &matrix,
                    gain: 1.0,
                    resampled: false,
                    samples: &samples,
                };
                mixer.mix([stream], frames, &mut out).expect("mixed");
                assert_eq!(
                    out, samples,
                    "{format:?} over {channels} channels, {frames} frames"
                );
            }
        }
    }
}

/// Every full-scale and boundary value of the narrow encodings, rather than
/// only the pseudo-random draws: an off-by-one in the saturation would
/// otherwise be found only by luck.
#[test]
fn the_extremes_of_every_encoding_survive_the_engine() {
    let cases: &[(SampleFormat, Vec<u8>)] = &[
        (SampleFormat::U8, vec![0x00, 0x01, 0x7F, 0x80, 0x81, 0xFF]),
        (
            SampleFormat::S16,
            [i16::MIN, -1, 0, 1, i16::MAX]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect(),
        ),
        (
            SampleFormat::S24,
            [-8_388_608i32, -1, 0, 1, 8_388_607]
                .iter()
                .flat_map(|v| v.to_le_bytes()[..3].to_vec())
                .collect(),
        ),
        (
            SampleFormat::S24In32,
            [-8_388_608i32, -1, 0, 1, 8_388_607]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect(),
        ),
        (
            SampleFormat::F32,
            [-1.0f32, -0.0, 0.0, 1.0, 0.5, -0.5]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect(),
        ),
    ];
    for (format, samples) in cases {
        let frames = samples.len() / format.bytes_per_sample();
        let out = through_the_engine(*format, 48_000, ChannelMap::MONO, frames, samples);
        assert_eq!(out, *samples, "{format:?} lost one of its extremes");
    }
}

/// The one encoding the pivot cannot carry whole. The crate documents the
/// loss, so the property states exactly what does survive rather than
/// pretending the claim covers it.
#[test]
fn a_thirty_two_bit_integer_source_keeps_its_top_twenty_four_bits() {
    let samples = probes(SampleFormat::S32, 64, &mut Prng::new(99));
    let out = through_the_engine(SampleFormat::S32, 48_000, ChannelMap::MONO, 64, &samples);
    assert_ne!(
        out, samples,
        "if this ever passes, the pivot widened and the documentation is stale"
    );
    for frame in 0..64 {
        let want = i32::from_le_bytes([
            samples[frame * 4],
            samples[frame * 4 + 1],
            samples[frame * 4 + 2],
            samples[frame * 4 + 3],
        ]);
        let got = i32::from_le_bytes([
            out[frame * 4],
            out[frame * 4 + 1],
            out[frame * 4 + 2],
            out[frame * 4 + 3],
        ]);
        let error = i64::from(got) - i64::from(want);
        assert!(
            error.abs() <= 128,
            "frame {frame}: {want} came back as {got}, which is more than the \
             low eight bits the pivot documents that it drops"
        );
    }
}

/// The claim names unity gain specifically, so the test states what a gain
/// that is *not* unity does rather than leaving it implied.
#[test]
fn a_gain_that_is_not_unity_changes_the_samples_as_it_should() {
    let map = ChannelMap::MONO;
    let matrix = ChannelMatrix::derive(&map, &map).expect("identity");
    let samples: Vec<u8> = [1_000i16, -2_000, 4_000, -8_000]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let mut mixer = Mixer::new(
        SinkFormat {
            format: SampleFormat::S16,
            rate: rate(48_000),
            channel_map: map,
        },
        4,
        1,
    )
    .expect("mixer");
    mixer.set_dither(Dither::None);
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
            4,
            &mut out,
        )
        .expect("mixed");
    let halved: Vec<u8> = [500i16, -1_000, 2_000, -4_000]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    assert_eq!(out, halved);
}
