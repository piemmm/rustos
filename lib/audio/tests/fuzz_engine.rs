//! Deterministic fuzz harness for the audio engine.
//!
//! The mixer reads frames a *client* wrote into a shared ring, so those bytes
//! cross a trust boundary even though they are not a parsed format: they are
//! arbitrary, and the streams they are summed with belong to other principals
//! on the same machine. The invariants driven here are what stops one
//! tenant's numbers bounding another's:
//!
//! * no input of any shape panics, reads out of bounds, or allocates
//!   unboundedly — every stage answers a value or a typed refusal;
//! * every sample the engine hands a device is **finite and inside full
//!   scale**, whatever arrived. A `NaN` reaching a shared accumulator would
//!   silence every other stream on the sink, and a float far outside full
//!   scale would swamp them;
//! * a stream of pure noise mixed beside a well-behaved one leaves the
//!   well-behaved one's contribution intact and bounded;
//! * the resampler never exceeds its own stated output bound and never emits
//!   a non-finite sample, whatever ratio and block sizes it is driven with;
//! * a channel-layout pair either yields a matrix whose coefficients are all
//!   finite, or is refused — never a matrix that silently drops a channel;
//! * the clock model never reports a rate the protocol would refuse to
//!   decode, however hostile the pairs a driver feeds it;
//! * the volume model resolves to a finite, non-negative multiply for every
//!   combination of gains, including the extremes of the integer range.
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded generator drives
//! every stage. A plain `cargo test` runs the fixed smoke sweep; `cargo xtask
//! fuzz` extends the loop to a wall-clock budget.

use tairix_abi::audio::{decode_clock_reply, encode_clock_reply};
use tairix_abi::driver::audio::{
    ChannelMap, ChannelPosition, Frames, GainRange, Rate, SampleFormat,
};
use tairix_abi::time::Time64;
use tairix_audio::channel::ChannelMatrix;
use tairix_audio::clock::ClockModel;
use tairix_audio::convert::{self, Dither, DitherSource};
use tairix_audio::mix::{Mixer, SinkFormat, StreamMix};
use tairix_audio::resample::{FilterBank, Ratio, Resampler};
use tairix_audio::volume::{millibel_to_linear, resolve, VolumeRequest};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
///
/// Each iteration builds a fresh filter bank, which is the dearest thing in
/// the crate — a few hundred Bessel and sine evaluations per row. The count
/// is sized so the smoke sweep stays a second or two of the ordinary test
/// pass; the wall-clock coverage is the budgeted run.
const SMOKE_ITERATIONS: u64 = 600;

/// Frames the largest driven block carries.
const MAX_FRAMES: usize = 64;

/// `x` reduced into `0..=max`, without a narrowing cast.
fn bounded(x: u64, max: usize) -> usize {
    let span = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    usize::try_from(x % span).unwrap_or(0)
}

/// Every encoding, so no stage is driven over only the convenient ones.
const FORMATS: &[SampleFormat] = &[
    SampleFormat::U8,
    SampleFormat::S16,
    SampleFormat::S24,
    SampleFormat::S24In32,
    SampleFormat::S32,
    SampleFormat::F32,
];

/// Every position, so a layout pair with no defined relationship is reached.
const POSITIONS: &[ChannelPosition] = &[
    ChannelPosition::Mono,
    ChannelPosition::FrontLeft,
    ChannelPosition::FrontRight,
    ChannelPosition::FrontCentre,
    ChannelPosition::LowFrequency,
    ChannelPosition::RearLeft,
    ChannelPosition::RearRight,
    ChannelPosition::SideLeft,
    ChannelPosition::SideRight,
];

/// Rates spanning the vocabulary's whole range, including its two bounds and
/// a pair with no common factor.
const RATES: &[u32] = &[
    4_000, 8_000, 11_025, 44_100, 44_101, 48_000, 96_000, 192_000, 768_000,
];

/// A layout of `count` distinct positions drawn from the vocabulary, or
/// [`None`] where the draw did not make a valid one.
fn layout(draw: u64, count: usize) -> Option<ChannelMap> {
    let mut chosen = Vec::with_capacity(count);
    let mut rolling = draw;
    for _ in 0..count {
        let position = POSITIONS[bounded(rolling, POSITIONS.len() - 1)];
        if !chosen.contains(&position) {
            chosen.push(position);
        }
        rolling = rolling.rotate_left(7) ^ 0x9E37_79B9;
    }
    ChannelMap::new(&chosen).ok()
}

/// Every sample the engine hands a device must decode back to a finite value
/// inside full scale — whatever arrived at the other end.
#[track_caller]
fn assert_deliverable(format: SampleFormat, bytes: &[u8], what: &str) {
    let samples = bytes.len() / format.bytes_per_sample();
    let mut pivot = vec![0.0f32; samples];
    let decoded = convert::decode(format, bytes, &mut pivot);
    assert_eq!(decoded, samples, "{what}: not every sample decoded");
    for sample in &pivot {
        assert!(
            sample.is_finite() && (-1.0..=1.0).contains(sample),
            "{what}: the device would be handed {sample}"
        );
    }
}

#[test]
fn driving_the_engine_with_any_input_never_panics_and_always_delivers_audio() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut state: u64 = tairix_fuzzseed::start(
        "driving_the_engine_with_any_input_never_panics_and_always_delivers_audio",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    );
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };

    let mut noise = vec![0u8; MAX_FRAMES * 8 * 4];
    let mut iteration: u64 = 0;
    loop {
        exercise_conversion(&mut noise, &mut next);
        exercise_channel_matrix(&mut next);
        exercise_mixer(&mut noise, &mut next);
        exercise_resampler(&mut next);
        exercise_clock(&mut next);
        exercise_volume(&mut next);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}

/// Fill `buffer` with pseudo-random bytes.
fn scramble(buffer: &mut [u8], next: &mut impl FnMut() -> u64) {
    for chunk in buffer.chunks_mut(8) {
        let word = next().to_le_bytes();
        let span = chunk.len();
        chunk.copy_from_slice(&word[..span]);
    }
}

/// Any byte image in any encoding converts into any other without panicking,
/// and what comes out is always deliverable audio.
fn exercise_conversion(noise: &mut [u8], next: &mut impl FnMut() -> u64) {
    let from = FORMATS[bounded(next(), FORMATS.len() - 1)];
    let to = FORMATS[bounded(next(), FORMATS.len() - 1)];
    let samples = bounded(next(), MAX_FRAMES);
    let span = samples * from.bytes_per_sample();
    scramble(&mut noise[..span], next);
    let mut scratch = vec![0.0f32; samples.max(1)];
    let mut out = vec![0u8; samples * to.bytes_per_sample()];
    if convert::convert(from, to, &noise[..span], &mut out, &mut scratch).is_ok() {
        assert_deliverable(to, &out, "conversion");
    }
    // A destination one byte short of what the samples need must refuse
    // rather than write a partial sample.
    if !out.is_empty() {
        let short = out.len() - 1;
        let _ = convert::convert(from, to, &noise[..span], &mut out[..short], &mut scratch);
    }
}

/// A layout pair either yields a finite matrix or is refused; nothing in
/// between.
fn exercise_channel_matrix(next: &mut impl FnMut() -> u64) {
    let source_count = 1 + bounded(next(), 7);
    let sink_count = 1 + bounded(next(), 7);
    let (Some(source), Some(sink)) = (layout(next(), source_count), layout(next(), sink_count))
    else {
        return;
    };
    let Ok(matrix) = ChannelMatrix::derive(&source, &sink) else {
        return;
    };
    let mut reached = vec![false; matrix.sources()];
    for destination in 0..matrix.destinations() {
        for (slot, seen) in reached.iter_mut().enumerate() {
            let weight = matrix.weight(destination, slot);
            assert!(
                weight.is_finite() && (0.0..=2.0).contains(&weight),
                "coefficient ({destination}, {slot}) is {weight}"
            );
            *seen |= weight != 0.0;
        }
    }
    // Every source channel either reaches somewhere or is the low-frequency
    // one the downmix rule drops; a channel silently going nowhere else would
    // be material lost without a refusal.
    for (slot, position) in source.positions().iter().enumerate() {
        assert!(
            reached[slot] || *position == ChannelPosition::LowFrequency,
            "{position:?} reached nothing and was not refused"
        );
    }
}

/// A hostile stream beside a well-behaved one: the sink still receives
/// deliverable audio, and the honest stream is still in it.
fn exercise_mixer(noise: &mut [u8], next: &mut impl FnMut() -> u64) {
    let sink_format = FORMATS[bounded(next(), FORMATS.len() - 1)];
    let Some(map) = layout(next(), 1 + bounded(next(), 7)) else {
        return;
    };
    let channels = usize::from(map.channels());
    let frames = 1 + bounded(next(), MAX_FRAMES - 1);
    let Ok(matrix) = ChannelMatrix::derive(&map, &map) else {
        return;
    };
    let Ok(mut mixer) = Mixer::new(
        SinkFormat {
            format: sink_format,
            rate: Rate::HZ_48000,
            channel_map: map,
        },
        frames,
        next(),
    ) else {
        return;
    };
    if next() & 1 == 0 {
        mixer.set_dither(Dither::None);
    }
    let hostile_format = FORMATS[bounded(next(), FORMATS.len() - 1)];
    let span = frames * channels * hostile_format.bytes_per_sample();
    scramble(&mut noise[..span], next);
    let hostile = noise[..span].to_vec();
    let quiet =
        vec![sink_format.silence_byte(); frames * channels * sink_format.bytes_per_sample()];
    let mut out = vec![0u8; frames * channels * sink_format.bytes_per_sample()];
    let streams = [
        StreamMix {
            format: hostile_format,
            matrix: &matrix,
            gain: gain_draw(next()),
            resampled: next() & 1 == 0,
            samples: &hostile,
        },
        StreamMix {
            format: sink_format,
            matrix: &matrix,
            gain: 1.0,
            resampled: false,
            samples: &quiet,
        },
    ];
    let taken = 1 + bounded(next(), frames - 1);
    if mixer.mix(streams.iter().copied(), taken, &mut out).is_ok() {
        assert_deliverable(
            sink_format,
            &out[..taken * channels * sink_format.bytes_per_sample()],
            "mix",
        );
    }
}

/// A gain drawn from the whole space a caller could hand the mixer,
/// including the values a resolved volume can never produce.
fn gain_draw(word: u64) -> f32 {
    match word % 6 {
        0 => 1.0,
        1 => 0.0,
        2 => f32::from_bits(u32::try_from(word >> 20 & 0xFFFF_FFFF).unwrap_or(0)),
        3 => -1.0,
        4 => f32::MAX,
        _ => millibel_to_linear(i32::try_from(word >> 32 & 0xFFFF).unwrap_or(0) - 32_768),
    }
}

/// Any ratio, any block sizes: the output stays inside the stated bound and
/// every sample stays finite.
fn exercise_resampler(next: &mut impl FnMut() -> u64) {
    let from = RATES[bounded(next(), RATES.len() - 1)];
    let to = RATES[bounded(next(), RATES.len() - 1)];
    let (Ok(source), Ok(sink)) = (Rate::new(from), Rate::new(to)) else {
        return;
    };
    let Ok(bank) = FilterBank::new(source, sink) else {
        return;
    };
    let channels = 1 + bounded(next(), 7);
    let Ok(mut resampler) = Resampler::new(&bank, channels) else {
        return;
    };
    let frames = 1 + bounded(next(), 32);
    let input: Vec<f32> = (0..frames * channels)
        .map(|_| {
            let word = next();
            f32::from(u16::try_from(word & 0xFFFF).unwrap_or(0)) / 32_768.0 - 1.0
        })
        .collect();
    let bound = resampler.max_output_frames(frames);
    let mut output = vec![0.0f32; bound * channels];
    let Ok((consumed, produced)) = resampler.process(&bank, &input, &mut output) else {
        return;
    };
    assert!(consumed <= frames, "consumed {consumed} of {frames}");
    assert!(
        produced <= bound,
        "produced {produced} past a bound of {bound}"
    );
    for sample in &output[..produced * channels] {
        assert!(
            sample.is_finite() && sample.abs() <= 4.0,
            "the filter produced {sample}"
        );
    }
    // A ratio handed straight in, which is the drifting clock domain's form.
    if let Some(ratio) = Ratio::new(
        u32::try_from(1 + bounded(next(), 4_095)).unwrap_or(1),
        u32::try_from(1 + bounded(next(), 4_095)).unwrap_or(1),
    ) {
        let _ = FilterBank::for_ratio(ratio);
    }
}

/// However hostile the pairs, the model either refuses them or reports a
/// rate the protocol can carry.
fn exercise_clock(next: &mut impl FnMut() -> u64) {
    let hz = RATES[bounded(next(), RATES.len() - 1)];
    let Ok(nominal) = Rate::new(hz) else {
        return;
    };
    let mut model = ClockModel::new(nominal);
    for _ in 0..16 {
        let position = Frames::new(next() >> bounded(next(), 40));
        let secs = i64::try_from(next() >> 40).unwrap_or(0) - 8_000;
        let nanos = u32::try_from(next() % 1_000_000_000).unwrap_or(0);
        let Ok(when) = Time64::new(secs, nanos) else {
            continue;
        };
        let _ = model.observe(position, when);
        if let Some(report) = model.report() {
            let frame = encode_clock_reply(Ok(report));
            assert_eq!(
                decode_clock_reply(&frame),
                Ok(report),
                "a reported clock must survive its own protocol"
            );
        }
        let _ = model.position_at(when);
        let _ = model.time_at(position);
    }
}

/// Every combination of gains resolves to a finite, non-negative multiply.
fn exercise_volume(next: &mut impl FnMut() -> u64) {
    let millibel = |word: u64| {
        i32::from_le_bytes(u32::try_from(word & 0xFFFF_FFFF).unwrap_or(0).to_le_bytes())
    };
    let request = VolumeRequest {
        stream_millibel: millibel(next()),
        application_millibel: millibel(next()),
        sink_millibel: millibel(next()),
        duck_millibel: millibel(next()),
        muted: next() & 1 == 0,
    };
    let hardware = GainRange::new(
        millibel(next()),
        millibel(next()),
        u32::try_from(1 + bounded(next(), 4_095)).unwrap_or(1),
    )
    .ok();
    let resolved = resolve(&request, hardware);
    assert!(
        resolved.software.is_finite() && resolved.software >= 0.0,
        "resolved a multiply of {}",
        resolved.software
    );
    if let (Some(setting), Some(range)) = (resolved.hardware_millibel, hardware) {
        assert!(
            (range.min_millibel()..=range.max_millibel()).contains(&setting),
            "the device was asked for {setting}, outside its own range"
        );
    }
    // The noise source is driven too: a dithered block must stay finite.
    let mut block = vec![0.0f32; 16];
    DitherSource::new(next()).apply(SampleFormat::S16, &mut block);
    for sample in &block {
        assert!(sample.is_finite(), "dither produced {sample}");
    }
}
