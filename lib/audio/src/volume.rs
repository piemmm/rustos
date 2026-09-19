//! The volume model: four independent gains resolved into one multiply and
//! one number a user interface can show.
//!
//! Per-stream, per-application, per-sink and the router's ducking all name a
//! gain in hundredths of a decibel, which is the unit a codec's own amplifier
//! capability word converts into without a scale factor. They sum — decibels
//! compose by addition — and the sum is split once between the hardware
//! control the device has and the multiply the mixer applies.
//!
//! # Unity is exactly one
//!
//! [`millibel_to_linear`] answers exactly `1.0` at zero, so an untouched
//! stream is multiplied by a number that changes nothing. That is not an
//! accident of the exponential's accuracy; it is a special case, because the
//! stack's bit-exactness claim rests on it.
//!
//! # Hardware takes the attenuation it can, software never amplifies to
//! compensate
//!
//! The hardware setting is rounded to the step *above* the target, so the
//! remainder software applies is always attenuation. Rounding the other way
//! would leave software making up the difference with gain, which costs
//! headroom on a path that has none to spare — and where the target lands on
//! the device's own grid (every whole decibel on most codecs) the software
//! remainder is zero and the multiply is exactly one.

use tairix_abi::driver::audio::GainRange;
use tairix_util::mathf;

/// The gain that changes nothing.
pub const UNITY_MILLIBEL: i32 = 0;

/// The quietest gain a user interface's taper reaches before mute.
///
/// Sixty decibels below unity is past the resolution of sixteen-bit material,
/// so nothing is audible below it and a taper that ran further would waste
/// half its travel on silence.
pub const DEFAULT_FLOOR_MILLIBEL: i32 = -6_000;

/// The attenuation a media stream takes while a conversation or assistive
/// output is live on the same sink.
///
/// Twenty decibels is the broadcast convention for speech over a bed: enough
/// that the speech is plainly dominant, little enough that the media has not
/// simply stopped.
pub const DUCK_MILLIBEL: i32 = -2_000;

/// `ln(10) / 2000`: the exponent one hundredth of a decibel contributes.
const MILLIBEL_EXPONENT: f64 = core::f64::consts::LN_10 / 2_000.0;

/// The linear multiplier `millibel` hundredths of a decibel name.
///
/// Exactly one at unity, and saturating at both ends like the maths module's
/// exponential: an absurd gain answers the largest finite multiplier rather
/// than an infinity that would spread through the mix.
#[must_use]
pub fn millibel_to_linear(millibel: i32) -> f32 {
    if millibel == UNITY_MILLIBEL {
        return 1.0;
    }
    as_f32(mathf::exp(f64::from(millibel) * MILLIBEL_EXPONENT))
}

/// The gain a user-interface control at `fraction` of its travel asks for,
/// with the quiet end at `floor`.
///
/// Linear in decibels, which is what a control labelled in decibels must be:
/// equal travel is equal perceived change, and the number beside the slider
/// is the number the mixer uses. A fraction at or below zero is the floor and
/// one at or above unity is [`UNITY_MILLIBEL`]; the caller decides whether the
/// very bottom of its control means mute, because that is a question about
/// the control rather than about the gain.
#[must_use]
pub fn fraction_to_millibel(fraction: f32, floor: i32) -> i32 {
    if fraction <= 0.0 || !fraction.is_finite() {
        return floor;
    }
    if fraction >= 1.0 {
        return UNITY_MILLIBEL;
    }
    let travel = f64::from(floor) * f64::from(1.0 - fraction);
    round_i32(travel)
}

/// The independent gains a stream's level is composed from.
///
/// Every field is a gain in hundredths of a decibel, and they sum. Keeping
/// them apart until the sum is what lets a user interface show the one it
/// owns while the mixer applies the total.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct VolumeRequest {
    /// What this stream asked for.
    pub stream_millibel: i32,
    /// What the application it belongs to is set to.
    pub application_millibel: i32,
    /// What the sink it lands on is set to.
    pub sink_millibel: i32,
    /// What the router's ducking rule imposes, which is zero or
    /// [`DUCK_MILLIBEL`].
    pub duck_millibel: i32,
    /// Whether the stream is muted, which is a state of its own rather than a
    /// very small gain: unmuting restores the level that was set.
    pub muted: bool,
}

impl VolumeRequest {
    /// The four gains' sum, saturating rather than wrapping so an absurd
    /// component cannot turn attenuation into gain.
    #[must_use]
    pub const fn total_millibel(&self) -> i32 {
        self.stream_millibel
            .saturating_add(self.application_millibel)
            .saturating_add(self.sink_millibel)
            .saturating_add(self.duck_millibel)
    }
}

/// The resolved level: what to program into the device, what the mixer
/// multiplies by, and the one number to show.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ResolvedVolume {
    /// The setting for the device's own control, where it has one.
    pub hardware_millibel: Option<i32>,
    /// The multiply the mix applies — exactly `1.0` when the hardware can
    /// deliver the whole of the requested gain, and exactly `0.0` when muted.
    pub software: f32,
    /// The gain actually delivered, hardware and software together. What a
    /// user interface shows, so it cannot disagree with what is heard.
    pub total_millibel: i32,
    /// Whether the stream is silent.
    pub muted: bool,
}

/// Split `request` between a device control of `hardware` and the mixer's
/// multiply.
///
/// A device with no control of its own takes the whole gain in software.
#[must_use]
pub fn resolve(request: &VolumeRequest, hardware: Option<GainRange>) -> ResolvedVolume {
    let total = request.total_millibel();
    if request.muted {
        return ResolvedVolume {
            // Attenuating the hardware as well means a muted stream is silent
            // even if the software path is bypassed later.
            hardware_millibel: hardware.map(|range| range.min_millibel()),
            software: 0.0,
            total_millibel: total,
            muted: true,
        };
    }
    let Some(range) = hardware else {
        return ResolvedVolume {
            hardware_millibel: None,
            software: millibel_to_linear(total),
            total_millibel: total,
            muted: false,
        };
    };
    let setting = step_at_or_above(total, range);
    ResolvedVolume {
        hardware_millibel: Some(setting),
        software: millibel_to_linear(total.saturating_sub(setting)),
        total_millibel: total,
        muted: false,
    }
}

/// The lowest setting on `range`'s own step grid that is at or above `target`,
/// clamped into the range.
///
/// At or above, so the software remainder is attenuation rather than gain.
fn step_at_or_above(target: i32, range: GainRange) -> i32 {
    let min = i64::from(range.min_millibel());
    let max = i64::from(range.max_millibel());
    let step = i64::from(range.step_millibel());
    let wanted = i64::from(target).clamp(min, max);
    let above = min + (wanted - min).div_euclid(step) * step;
    let snapped = if above < wanted { above + step } else { above };
    // The step grid can overshoot the top of the range by less than one step,
    // in which case the loudest setting is the closest the device has.
    i32::try_from(snapped.min(max)).unwrap_or(range.max_millibel())
}

/// `value` as a linear multiplier.
///
/// Clamped into the narrower type's own range as well as the exponential's:
/// the largest double is past the largest float, so an absurd gain would
/// otherwise become an infinity and spread through everything the mix
/// multiplies it into.
#[allow(
    clippy::cast_possible_truncation,
    reason = "the value is clamped into f32's range on the line above, so the \
              truncation the lint warns about cannot occur"
)]
fn as_f32(value: f64) -> f32 {
    mathf::clamp(value, 0.0, f64::from(f32::MAX)) as f32
}

/// `value` rounded to the nearest whole hundredth of a decibel.
fn round_i32(value: f64) -> i32 {
    mathf::round_i32(value)
}

#[cfg(test)]
#[path = "volume_tests.rs"]
mod tests;
