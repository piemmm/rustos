//! Unit tests for the resampler.
//!
//! The filter's figures are **measured** here: the bank's own coefficients are
//! reassembled into the prototype they were sliced from and its frequency
//! response is evaluated, so the numbers the crate documents cannot drift
//! away from the filter it ships.

use alloc::vec;
use alloc::vec::Vec;

use super::{
    FilterBank, Ratio, Resampler, BASE_TAPS, DESIGN_ATTENUATION_DB, MAX_PHASES, MAX_TAPS,
    PASSBAND_EDGE, STOPBAND_EDGE,
};
use tairix_abi::driver::audio::Rate;
use tairix_abi::Errno;
use tairix_util::mathf;

fn rate(hz: u32) -> Rate {
    Rate::new(hz).expect("a rate inside the vocabulary")
}

/// The prototype lowpass the bank's rows were sliced from, at the upsampled
/// rate of `phases` samples per input frame.
///
/// Row `p`'s tap `j` is the kernel at `p/P + Q/2 - 1 - j` input frames from
/// the centre, so it belongs at prototype index `p + P*(Q-1-j)`. Rows `0` to
/// `P-1` tile the prototype exactly; the extra end row duplicates what row
/// zero already covers one frame along, and the window is zero at the single
/// index past them.
fn prototype(bank: &FilterBank) -> Vec<f64> {
    let phases = usize::try_from(bank.phases()).expect("a small phase count");
    let span = bank.taps();
    let mut taps = vec![0.0f64; phases * span];
    for phase in 0..phases {
        let row = bank.row(u32::try_from(phase).expect("in range"));
        for (j, coefficient) in row.iter().enumerate() {
            taps[phase + phases * (span - 1 - j)] = f64::from(*coefficient);
        }
    }
    taps
}

/// The prototype's gain at `frequency` cycles per **input** frame, relative to
/// its own gain at zero.
fn response(taps: &[f64], phases: f64, frequency: f64) -> f64 {
    let mut real = 0.0;
    let mut imaginary = 0.0;
    for (index, tap) in taps.iter().enumerate() {
        let position = f64::from(u32::try_from(index).expect("in range")) / phases;
        let angle = -core::f64::consts::TAU * frequency * position;
        real += tap * mathf::cos(angle);
        imaginary += tap * mathf::sin(angle);
    }
    mathf::hypot(real, imaginary) / phases
}

/// Twenty times the base-ten logarithm of `gain`, by the natural log the
/// maths module offers.
fn decibels(gain: f64) -> f64 {
    if gain <= 0.0 {
        return -400.0;
    }
    // `ln` is not in the shared module, and only the tests need it, so the
    // logarithm is recovered from the exponential by bisection over the
    // range a filter response can occupy.
    let (mut low, mut high) = (-400.0f64, 40.0f64);
    for _ in 0..200 {
        let middle = f64::midpoint(low, high);
        if mathf::exp(middle * core::f64::consts::LN_10 / 20.0) < gain {
            low = middle;
        } else {
            high = middle;
        }
    }
    f64::midpoint(low, high)
}

/// The worst passband deviation and the worst stopband leak, in decibels, for
/// a bank whose design cutoff is `cutoff` cycles per input frame.
fn measure(bank: &FilterBank, cutoff: f64) -> (f64, f64) {
    let taps = prototype(bank);
    let phases = f64::from(bank.phases());
    let stop = cutoff * STOPBAND_EDGE / PASSBAND_EDGE;
    let mut ripple: f64 = 0.0;
    for step in 0..=120 {
        let frequency = cutoff * f64::from(step) / 120.0;
        let deviation = mathf::fabs(decibels(response(&taps, phases, frequency)));
        ripple = ripple.max(deviation);
    }
    let mut leak = f64::MIN;
    let top = phases / 2.0;
    for step in 0..=160 {
        let frequency = stop + (top - stop) * f64::from(step) / 160.0;
        leak = leak.max(decibels(response(&taps, phases, frequency)));
    }
    (ripple, -leak)
}

#[test]
fn the_ratio_reduces_to_lowest_terms() {
    let ratio = Ratio::between(rate(48_000), rate(44_100));
    assert_eq!((ratio.input(), ratio.output()), (160, 147));
    let back = Ratio::between(rate(44_100), rate(48_000));
    assert_eq!((back.input(), back.output()), (147, 160));
    assert_eq!(
        Ratio::between(rate(8_000), rate(48_000)),
        Ratio::new(1, 6).expect("valid")
    );
    assert!(Ratio::between(rate(48_000), rate(48_000)).is_unity());
    assert_eq!(Ratio::new(0, 1), None);
    assert_eq!(Ratio::new(1, 0), None);
}

/// A windowed sinc at unity is very nearly the identity, and "very nearly"
/// would destroy the stack's bit-exactness property.
#[test]
fn an_equal_rate_pair_copies_rather_than_filters() {
    let bank = FilterBank::new(rate(48_000), rate(48_000)).expect("unity bank");
    assert!(bank.is_unity() && bank.is_exact());
    let mut resampler = Resampler::new(&bank, 2).expect("stereo");
    assert_eq!(resampler.latency_frames(), 0);
    let input: Vec<f32> = (0..64u8).map(|n| f32::from(n) / 64.0 - 0.5).collect();
    let mut output = vec![0.0f32; 64];
    let (consumed, produced) = resampler
        .process(&bank, &input, &mut output)
        .expect("whole frames");
    assert_eq!((consumed, produced), (32, 32));
    assert_eq!(output, input);
}

#[test]
fn the_standard_rate_family_gets_an_exact_phase_bank() {
    for (from, to, phases) in [
        (44_100u32, 48_000u32, 160u32),
        (48_000, 44_100, 147),
        (96_000, 44_100, 147),
        (8_000, 48_000, 6),
        (96_000, 48_000, 1),
    ] {
        let bank = FilterBank::new(rate(from), rate(to)).expect("bank");
        assert!(
            bank.is_exact(),
            "{from} into {to} should land on a row every time"
        );
        assert_eq!(bank.phases(), phases, "{from} into {to}");
    }
}

/// A rate pair with no common factor needs more rows than the bank holds, so
/// it reaches its phase by interpolating between the two nearest — the
/// fractional-delay case.
#[test]
fn a_coprime_rate_pair_falls_back_to_the_interpolated_bank() {
    let bank = FilterBank::new(rate(44_101), rate(48_000)).expect("bank");
    assert!(!bank.is_exact());
    assert_eq!(bank.phases(), MAX_PHASES);
    // It still resamples: the stepping is the same exact rational either way.
    let mut resampler = Resampler::new(&bank, 1).expect("mono");
    let input = vec![0.25f32; 512];
    let mut output = vec![0.0f32; 1_024];
    let (consumed, produced) = resampler
        .process(&bank, &input, &mut output)
        .expect("frames");
    assert_eq!(consumed, 512);
    assert!(produced > 400, "produced {produced}");
}

/// The whole point of carrying the position as an integer pair: an hour of
/// playback ends where the arithmetic says, not a few samples either side.
#[test]
fn the_position_never_drifts_from_the_exact_rational() {
    let bank = FilterBank::new(rate(48_000), rate(44_100)).expect("bank");
    let mut resampler = Resampler::new(&bank, 1).expect("mono");
    let mut produced_total = 0u64;
    let mut consumed_total = 0u64;
    let input = vec![0.0f32; 4_096];
    let mut output = vec![0.0f32; 4_096];
    for _ in 0..64 {
        let (consumed, produced) = resampler
            .process(&bank, &input, &mut output)
            .expect("frames");
        consumed_total += u64::try_from(consumed).expect("small");
        produced_total += u64::try_from(produced).expect("small");
    }
    // 262 144 input frames at 160/147 is 240 837.6 output frames; the filter's
    // own group delay accounts for the shortfall and nothing else does.
    let ideal = consumed_total * 147 / 160;
    let shortfall = ideal - produced_total;
    assert!(
        shortfall <= u64::try_from(bank.taps()).expect("small"),
        "produced {produced_total} of an ideal {ideal}: the gap must be the \
         filter's group delay alone, not an accumulating drift"
    );
}

/// The filter's ramp-in and ramp-out, in **output** frames: the zeroed
/// history is still inside the window there, so the level is legitimately
/// climbing and nothing about the steady state can be read from it.
fn settling(bank: &FilterBank) -> usize {
    let ratio = bank.ratio();
    bank.taps() * usize::try_from(ratio.output()).unwrap_or(1)
        / usize::try_from(ratio.input()).unwrap_or(1)
        + bank.taps()
}

#[test]
fn a_constant_input_comes_out_at_the_same_level() {
    for (from, to) in [(44_100u32, 48_000u32), (48_000, 44_100), (8_000, 48_000)] {
        let bank = FilterBank::new(rate(from), rate(to)).expect("bank");
        let skip = settling(&bank);
        let mut resampler = Resampler::new(&bank, 1).expect("mono");
        let input = vec![0.5f32; 16_384];
        let mut output = vec![0.0f32; 32_768];
        let (_, produced) = resampler
            .process(&bank, &input, &mut output)
            .expect("frames");
        assert!(produced > 2 * skip, "{from} into {to} produced too little");
        for sample in &output[skip..produced - skip] {
            assert!(
                (sample - 0.5).abs() < 1e-4,
                "{from} into {to} moved a constant 0.5 to {sample}"
            );
        }
    }
}

/// The documented figures, held against the filter the crate actually builds.
#[test]
fn the_upsampling_kernel_meets_its_documented_passband_and_stopband() {
    let bank = FilterBank::new(rate(24_000), rate(48_000)).expect("bank");
    let (ripple, attenuation) = measure(&bank, PASSBAND_EDGE);
    assert!(
        ripple < 0.1,
        "passband ripple measured {ripple} dB, which is not flat"
    );
    assert!(
        attenuation >= DESIGN_ATTENUATION_DB,
        "stopband measured {attenuation} dB against a documented \
         {DESIGN_ATTENUATION_DB} dB"
    );
}

#[test]
fn the_downsampling_kernel_moves_its_cutoff_down_with_the_output_rate() {
    let bank = FilterBank::new(rate(48_000), rate(24_000)).expect("bank");
    // Halving the rate halves the band that survives, measured against the
    // input rate the prototype is expressed in.
    let (ripple, attenuation) = measure(&bank, PASSBAND_EDGE / 2.0);
    assert!(ripple < 0.1, "passband ripple measured {ripple} dB");
    assert!(
        attenuation >= DESIGN_ATTENUATION_DB,
        "stopband measured {attenuation} dB against a documented \
         {DESIGN_ATTENUATION_DB} dB"
    );
}

/// The transition is where the response leaves the passband and reaches the
/// stopband; the two edges the crate documents must bracket it.
#[test]
fn the_transition_lies_between_the_two_documented_edges() {
    let bank = FilterBank::new(rate(24_000), rate(48_000)).expect("bank");
    let taps = prototype(&bank);
    let phases = f64::from(bank.phases());
    let at_passband = decibels(response(&taps, phases, PASSBAND_EDGE));
    let at_stopband = decibels(response(&taps, phases, STOPBAND_EDGE));
    assert!(
        at_passband > -1.0,
        "the response has already fallen {at_passband} dB at the passband edge"
    );
    assert!(
        at_stopband < -DESIGN_ATTENUATION_DB,
        "the response is still {at_stopband} dB at the stopband edge"
    );
}

#[test]
fn a_tone_inside_the_passband_survives_and_one_above_nyquist_does_not() {
    let bank = FilterBank::new(rate(48_000), rate(24_000)).expect("bank");
    let mut resampler = Resampler::new(&bank, 1).expect("mono");
    // A tone at a tenth of the input rate is well inside the surviving band.
    let input: Vec<f32> = (0..8_192)
        .map(|n: u32| {
            let phase = core::f64::consts::TAU * 0.1 * f64::from(n);
            as_f32(mathf::sin(phase)) * 0.5
        })
        .collect();
    let mut output = vec![0.0f32; 8_192];
    let (_, produced) = resampler
        .process(&bank, &input, &mut output)
        .expect("frames");
    let skip = settling(&bank);
    // Root-mean-square rather than peak: a decimated sine is rarely sampled
    // at its own crest, so a peak would measure the sampling phase as much as
    // the filter.
    let level = rms(&output[skip..produced - skip]);
    assert!(
        (level - 0.5 / core::f32::consts::SQRT_2).abs() < 0.005,
        "the tone came through at {level}"
    );

    // A tone above the new Nyquist would alias if it were not filtered out.
    resampler.reset();
    let above: Vec<f32> = (0..8_192)
        .map(|n: u32| {
            let phase = core::f64::consts::TAU * 0.35 * f64::from(n);
            as_f32(mathf::sin(phase)) * 0.5
        })
        .collect();
    let mut folded = vec![0.0f32; 8_192];
    let (_, produced) = resampler
        .process(&bank, &above, &mut folded)
        .expect("frames");
    let level = rms(&folded[skip..produced - skip]);
    assert!(
        level < 1e-4,
        "a tone above the new Nyquist aliased back at {level}"
    );
}

/// The root-mean-square level of a settled block.
fn rms(block: &[f32]) -> f32 {
    let mut sum = 0.0f64;
    for sample in block {
        sum += f64::from(*sample) * f64::from(*sample);
    }
    let count = f64::from(u32::try_from(block.len()).unwrap_or(1).max(1));
    as_f32(mathf::sqrt(sum / count))
}

/// `value` as a sample.
#[allow(
    clippy::cast_possible_truncation,
    reason = "a test signal in minus one to one, where the pivot is exact to \
              far more digits than the assertions read"
)]
fn as_f32(value: f64) -> f32 {
    value as f32
}

#[test]
fn a_short_destination_stops_consumption_rather_than_dropping_input() {
    let bank = FilterBank::new(rate(24_000), rate(48_000)).expect("bank");
    let mut resampler = Resampler::new(&bank, 1).expect("mono");
    let input = vec![0.25f32; 4_096];
    let mut output = vec![0.0f32; 64];
    let (consumed, produced) = resampler
        .process(&bank, &input, &mut output)
        .expect("frames");
    assert_eq!(produced, 64);
    assert!(
        consumed < 4_096,
        "a full destination must stop consumption, not swallow the rest"
    );
}

#[test]
fn a_partial_frame_on_either_side_is_refused() {
    let bank = FilterBank::new(rate(24_000), rate(48_000)).expect("bank");
    let mut resampler = Resampler::new(&bank, 3).expect("three channels");
    let mut output = vec![0.0f32; 9];
    assert_eq!(
        resampler.process(&bank, &[0.0; 4], &mut output),
        Err(Errno::LengthOutOfRange)
    );
    let mut ragged = vec![0.0f32; 8];
    assert_eq!(
        resampler.process(&bank, &[0.0; 6], &mut ragged),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn an_impossible_channel_count_is_refused() {
    let bank = FilterBank::new(rate(24_000), rate(48_000)).expect("bank");
    assert_eq!(Resampler::new(&bank, 0).err(), Some(Errno::OutOfRange));
    assert_eq!(Resampler::new(&bank, 9).err(), Some(Errno::OutOfRange));
}

#[test]
fn channels_stay_independent_through_the_filter() {
    let bank = FilterBank::new(rate(24_000), rate(48_000)).expect("bank");
    let mut resampler = Resampler::new(&bank, 2).expect("stereo");
    // One channel constant, the other silent: a mis-strided history would
    // leak between them immediately.
    let mut input = vec![0.0f32; 4_096];
    for frame in 0..2_048 {
        input[frame * 2] = 0.5;
    }
    let mut output = vec![0.0f32; 8_192];
    let (_, produced) = resampler
        .process(&bank, &input, &mut output)
        .expect("frames");
    let skip = settling(&bank);
    for frame in skip..produced - skip {
        assert!((output[frame * 2] - 0.5).abs() < 1e-3, "left moved");
        assert!(output[frame * 2 + 1].abs() < 1e-6, "right picked up left");
    }
}

#[test]
fn a_reset_returns_the_filter_to_the_start_of_a_stream() {
    let bank = FilterBank::new(rate(24_000), rate(48_000)).expect("bank");
    let mut resampler = Resampler::new(&bank, 1).expect("mono");
    let input: Vec<f32> = (0..256u16).map(|n| f32::from(n) / 512.0).collect();
    let mut first = vec![0.0f32; 1_024];
    let (_, produced) = resampler
        .process(&bank, &input, &mut first)
        .expect("frames");
    resampler.reset();
    let mut again = vec![0.0f32; 1_024];
    let (_, repeated) = resampler
        .process(&bank, &input, &mut again)
        .expect("frames");
    assert_eq!(produced, repeated);
    assert_eq!(first[..produced], again[..repeated]);
}

#[test]
fn the_output_bound_is_never_exceeded() {
    for (from, to) in [(44_100u32, 48_000u32), (48_000, 44_100), (8_000, 48_000)] {
        let bank = FilterBank::new(rate(from), rate(to)).expect("bank");
        let mut resampler = Resampler::new(&bank, 1).expect("mono");
        let bound = resampler.max_output_frames(1_024);
        let input = vec![0.1f32; 1_024];
        let mut output = vec![0.0f32; bound];
        let (consumed, produced) = resampler
            .process(&bank, &input, &mut output)
            .expect("frames");
        assert_eq!(consumed, 1_024, "{from} into {to} left input behind");
        assert!(produced <= bound, "{from} into {to}");
    }
}

/// A decimating ratio must reach the stopband by the *output's* Nyquist, so
/// it needs proportionally more taps than an interpolating one.
#[test]
fn the_tap_count_scales_with_decimation_and_stops_at_the_cap() {
    for (from, to) in [(24_000u32, 48_000u32), (44_100, 48_000), (48_000, 48_000)] {
        let bank = FilterBank::new(rate(from), rate(to)).expect("bank");
        let taps = if bank.is_unity() { 0 } else { BASE_TAPS };
        assert_eq!(bank.taps(), taps, "{from} into {to} should keep the base");
    }
    // Four to one needs four times the span.
    let quarter = FilterBank::new(rate(192_000), rate(48_000)).expect("bank");
    assert_eq!(quarter.taps(), 4 * BASE_TAPS);
    // A little over one to one needs a little more than the base.
    let gentle = FilterBank::new(rate(48_000), rate(44_100)).expect("bank");
    assert!(gentle.taps() > BASE_TAPS && gentle.taps() < 2 * BASE_TAPS);
    // And an extreme ratio stops at the cap rather than unbounding the work
    // one output sample costs.
    let extreme = FilterBank::new(rate(768_000), rate(4_000)).expect("bank");
    assert_eq!(extreme.taps(), MAX_TAPS);
}

/// The bank is shared per rate pair, so its worst case bounds what one pair
/// costs however hostile the rates a client asks for.
#[test]
fn the_widest_bank_any_rate_pair_can_ask_for_stays_bounded() {
    let rows = usize::try_from(MAX_PHASES).expect("small") + 1;
    let worst = rows * MAX_TAPS * 4;
    assert!(
        worst < 400 * 1024,
        "a bank could reach {worst} bytes, which is no longer a bound"
    );
}

#[test]
fn a_bank_of_another_ratio_is_refused_rather_than_filtered_over() {
    // The history is sized and stepped for the ratio the resampler was built
    // for, so driving it over a foreign bank would produce wrong audio with
    // no error anywhere. Refused instead.
    let bank = FilterBank::new(rate(24_000), rate(48_000)).expect("bank");
    let other = FilterBank::new(rate(44_100), rate(48_000)).expect("other bank");
    let mut resampler = Resampler::new(&bank, 1).expect("mono");
    assert_eq!(resampler.ratio(), bank.ratio());
    let mut output = vec![0.0f32; 64];
    assert_eq!(
        resampler.process(&other, &[0.0; 16], &mut output),
        Err(Errno::NotSupported)
    );
    assert!(resampler.process(&bank, &[0.0; 16], &mut output).is_ok());
}
