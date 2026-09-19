//! Unit tests for sample conversion.
//!
//! The round-trip exactness tests are the foundation the whole stack's
//! bit-exactness claim stands on, so they are exhaustive where the encoding's
//! value space allows it rather than sampled.

// Exactness is the property under test in this module: a tolerance would
// accept precisely the imprecision the assertions exist to forbid.
#![allow(clippy::float_cmp)]

use alloc::vec;
use alloc::vec::Vec;

use tairix_cpuops::{CpuFeatureSet, DecisionReason};

use super::{convert, decode, encode, kernel, narrows, resolve, silence, DitherSource, PORTABLE};
use tairix_abi::driver::audio::SampleFormat;
use tairix_abi::Errno;

/// Every encoding, so a test that must hold for all of them says so rather
/// than listing five and forgetting the sixth.
const ALL: &[SampleFormat] = &[
    SampleFormat::U8,
    SampleFormat::S16,
    SampleFormat::S24,
    SampleFormat::S24In32,
    SampleFormat::S32,
    SampleFormat::F32,
];

/// Decode `bytes` and re-encode into the same format.
fn round_trip(format: SampleFormat, bytes: &[u8]) -> Vec<u8> {
    let samples = bytes.len() / format.bytes_per_sample();
    let mut pivot = vec![0.0f32; samples];
    let decoded = decode(format, bytes, &mut pivot);
    assert_eq!(decoded, samples, "every whole sample must decode");
    let mut out = vec![0u8; bytes.len()];
    let written = encode(format, &pivot, &mut out);
    assert_eq!(written, bytes.len());
    out
}

#[test]
fn every_unsigned_byte_survives_the_pivot_unchanged() {
    let all: Vec<u8> = (0..=255u8).collect();
    assert_eq!(round_trip(SampleFormat::U8, &all), all);
}

#[test]
fn every_sixteen_bit_sample_survives_the_pivot_unchanged() {
    let mut bytes = Vec::with_capacity(65_536 * 2);
    for value in i16::MIN..=i16::MAX {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    assert_eq!(round_trip(SampleFormat::S16, &bytes), bytes);
}

/// Twenty-four bits is four million too many to enumerate, so the extremes,
/// the powers of two either side of every mantissa boundary, and a stride
/// across the range stand in for them.
fn twenty_four_bit_probes() -> Vec<i32> {
    let mut values = vec![-8_388_608, -8_388_607, -1, 0, 1, 8_388_606, 8_388_607];
    for shift in 0..24 {
        values.push(1 << shift);
        values.push(-(1 << shift));
        values.push((1 << shift) - 1);
    }
    let mut value = -8_388_608i32;
    while value < 8_388_607 {
        values.push(value);
        value += 4_099;
    }
    values.retain(|v| (-8_388_608..=8_388_607).contains(v));
    values
}

#[test]
fn packed_twenty_four_bit_samples_survive_the_pivot_unchanged() {
    let mut bytes = Vec::new();
    for value in twenty_four_bit_probes() {
        bytes.extend_from_slice(&value.to_le_bytes()[..3]);
    }
    assert_eq!(round_trip(SampleFormat::S24, &bytes), bytes);
}

#[test]
fn twenty_four_bit_in_thirty_two_survives_the_pivot_unchanged() {
    let mut bytes = Vec::new();
    for value in twenty_four_bit_probes() {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    assert_eq!(round_trip(SampleFormat::S24In32, &bytes), bytes);
}

#[test]
fn float_samples_survive_the_pivot_unchanged_including_negative_zero() {
    let probes: &[f32] = &[
        0.0,
        -0.0,
        1.0,
        -1.0,
        0.5,
        -0.5,
        1.0 / 3.0,
        -1.0 / 3.0,
        f32::MIN_POSITIVE,
    ];
    let mut bytes = Vec::new();
    for value in probes {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    let back = round_trip(SampleFormat::F32, &bytes);
    assert_eq!(back, bytes, "a float source must read back bit-identically");
    // Negative zero specifically: it is the one value a careless accumulate
    // or clamp turns into its positive twin.
    assert_eq!(&back[4..8], &(-0.0f32).to_le_bytes());
}

/// The one encoding the pivot cannot carry whole. The crate documents the
/// loss, so the test states exactly what survives rather than skipping it.
#[test]
fn thirty_two_bit_integers_keep_their_top_twenty_four_bits_and_no_more() {
    let probes: &[i32] = &[0, 1, -1, i32::MIN, i32::MAX, 0x1234_5678, -0x1234_5678];
    for value in probes {
        let bytes = value.to_le_bytes();
        let back = round_trip(SampleFormat::S32, &bytes);
        let recovered = i32::from_le_bytes([back[0], back[1], back[2], back[3]]);
        let error = i64::from(recovered) - i64::from(*value);
        assert!(
            error.abs() <= 128,
            "S32 {value} came back as {recovered}: the pivot's twenty-four \
             bits bound the error to half a step of the low eight"
        );
    }
    // The extremes still saturate to themselves rather than wrapping.
    assert_eq!(
        round_trip(SampleFormat::S32, &i32::MIN.to_le_bytes()),
        i32::MIN.to_le_bytes()
    );
    assert_eq!(
        round_trip(SampleFormat::S32, &i32::MAX.to_le_bytes()),
        i32::MAX.to_le_bytes()
    );
}

#[test]
fn a_sample_past_full_scale_saturates_rather_than_wrapping() {
    for format in ALL {
        let mut out = vec![0u8; format.bytes_per_sample() * 2];
        encode(*format, &[8.0, -8.0], &mut out);
        let mut back = vec![0.0f32; 2];
        decode(*format, &out, &mut back);
        assert!(
            back[0] > 0.9 && back[1] < -0.9,
            "{format:?} wrapped instead of saturating: {back:?}"
        );
    }
}

/// A `NaN` in a shared accumulator silences every other stream on the sink,
/// so it is stopped at the boundary rather than summed.
#[test]
fn a_non_finite_float_sample_decodes_as_silence() {
    let mut bytes = Vec::new();
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    let mut pivot = vec![9.0f32; 3];
    decode(SampleFormat::F32, &bytes, &mut pivot);
    assert_eq!(pivot, vec![0.0, 0.0, 0.0]);
}

#[test]
fn a_float_sample_past_full_scale_is_bounded_at_full_scale() {
    let mut bytes = Vec::new();
    for value in [1000.0f32, -1000.0f32] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    let mut pivot = vec![0.0f32; 2];
    decode(SampleFormat::F32, &bytes, &mut pivot);
    assert_eq!(pivot, vec![1.0, -1.0]);
}

/// Two hundred and fifty-six times full scale is what an unclamped
/// twenty-four-in-thirty-two sample would reach, which would swamp every
/// other stream on the sink.
#[test]
fn a_twenty_four_in_thirty_two_sample_outside_its_own_range_is_bounded() {
    let mut bytes = Vec::new();
    for value in [i32::MAX, i32::MIN] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    let mut pivot = vec![0.0f32; 2];
    decode(SampleFormat::S24In32, &bytes, &mut pivot);
    assert!(pivot[0] <= 1.0 && pivot[1] >= -1.0, "{pivot:?}");
}

#[test]
fn silence_is_the_encodings_own_quiet_value() {
    for format in ALL {
        let mut out = vec![0x5Au8; format.bytes_per_sample() * 4];
        silence(*format, &mut out);
        let mut pivot = vec![9.0f32; 4];
        decode(*format, &out, &mut pivot);
        assert_eq!(pivot, vec![0.0; 4], "{format:?} silence is not silent");
    }
    // The unsigned encoding specifically: a zero byte there is full negative
    // deflection, which is a click rather than a gap.
    let mut out = [0u8; 4];
    silence(SampleFormat::U8, &mut out);
    assert_eq!(out, [0x80; 4]);
}

#[test]
fn conversion_is_total_over_every_pair() {
    let source: Vec<u8> = (0..64u8).collect();
    let mut scratch = vec![0.0f32; 64];
    for from in ALL {
        for to in ALL {
            let samples = source.len() / from.bytes_per_sample();
            let mut out = vec![0u8; samples * to.bytes_per_sample()];
            let written = convert(*from, *to, &source, &mut out, &mut scratch)
                .expect("a sized destination is never refused");
            assert_eq!(
                written,
                samples * to.bytes_per_sample(),
                "{from:?} to {to:?}"
            );
        }
    }
}

#[test]
fn converting_between_equal_encodings_is_a_copy() {
    let source: Vec<u8> = (0..96u8).collect();
    let mut scratch = vec![0.0f32; 96];
    for format in ALL {
        let samples = source.len() / format.bytes_per_sample();
        let bytes = samples * format.bytes_per_sample();
        let mut out = vec![0u8; bytes];
        convert(*format, *format, &source, &mut out, &mut scratch).expect("sized");
        assert_eq!(out, source[..bytes], "{format:?} did not pass through");
    }
}

#[test]
fn a_short_destination_or_an_empty_scratch_is_refused() {
    let source = [0u8; 16];
    let mut scratch = vec![0.0f32; 8];
    let mut out = [0u8; 1];
    assert_eq!(
        convert(
            SampleFormat::S16,
            SampleFormat::S32,
            &source,
            &mut out,
            &mut scratch
        ),
        Err(Errno::BufferTooSmall)
    );
    let mut sized = [0u8; 64];
    assert_eq!(
        convert(
            SampleFormat::S16,
            SampleFormat::S32,
            &source,
            &mut sized,
            &mut []
        ),
        Err(Errno::BufferTooSmall)
    );
}

#[test]
fn the_dispatch_family_self_verifies_and_resolves_to_the_portable_kernel() {
    let decision = resolve(CpuFeatureSet::EMPTY);
    assert_eq!(decision.chosen, super::BASELINE_NAME);
    assert_eq!(decision.reason, DecisionReason::Baseline);
    // The self-verify ran against real vectors, so the baseline is not merely
    // the last resort — it reproduced the reference on every one of them.
    let resolved = kernel();
    let bytes = [0x01, 0x80, 0xFF, 0x7F];
    let mut theirs = vec![0.0f32; 2];
    let mut ours = vec![0.0f32; 2];
    (resolved.decode)(SampleFormat::S16, &bytes, &mut theirs);
    (PORTABLE.decode)(SampleFormat::S16, &bytes, &mut ours);
    assert_eq!(theirs, ours);
}

#[test]
fn dither_belongs_only_where_the_destination_is_narrower() {
    assert!(narrows(24, SampleFormat::S16));
    assert!(narrows(32, SampleFormat::S24));
    assert!(!narrows(16, SampleFormat::S16));
    assert!(!narrows(8, SampleFormat::S24));
    // A float destination has no step, so nothing is thrown away and nothing
    // is dithered.
    assert!(!narrows(32, SampleFormat::F32));
}

#[test]
fn triangular_dither_is_bounded_by_one_destination_step_and_centred() {
    let mut noise = DitherSource::new(0x5EED);
    let mut block = vec![0.0f32; 4_096];
    noise.apply(SampleFormat::S16, &mut block);
    let step = 1.0f32 / 32_768.0;
    let mut sum = 0.0f64;
    for sample in &block {
        assert!(
            sample.abs() <= step,
            "a triangular draw must stay inside one step: {sample}"
        );
        sum += f64::from(*sample);
    }
    let mean = sum / 4_096.0;
    assert!(
        mean.abs() < f64::from(step) * 0.05,
        "the noise must be centred on zero, not offset: {mean}"
    );
}

#[test]
fn dither_is_deterministic_for_a_seed_and_different_across_seeds() {
    let mut first = vec![0.0f32; 32];
    let mut again = vec![0.0f32; 32];
    let mut other = vec![0.0f32; 32];
    DitherSource::new(7).apply(SampleFormat::S16, &mut first);
    DitherSource::new(7).apply(SampleFormat::S16, &mut again);
    DitherSource::new(8).apply(SampleFormat::S16, &mut other);
    assert_eq!(first, again);
    assert_ne!(first, other);
}

#[test]
fn a_float_destination_is_never_dithered() {
    let mut block = vec![0.25f32; 16];
    DitherSource::new(1).apply(SampleFormat::F32, &mut block);
    assert_eq!(block, vec![0.25f32; 16]);
}

#[test]
fn a_partial_trailing_sample_is_left_undecoded_rather_than_read_past() {
    // Five bytes is one whole packed twenty-four-bit sample and two spare.
    let bytes = [0x11, 0x22, 0x33, 0x44, 0x55];
    let mut pivot = vec![9.0f32; 4];
    assert_eq!(decode(SampleFormat::S24, &bytes, &mut pivot), 1);
    assert_eq!(pivot[1], 9.0, "the untouched slots stay untouched");
}
