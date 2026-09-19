//! The device clock model: what a sound card's rate *actually* is, rather
//! than what its crystal is labelled.
//!
//! Every period a driver reports the pair (device frame position, the time it
//! was sampled at). Fitting a line through a window of those pairs gives the
//! device's real rate — 47 998.6 Hz for a card that says 48 000 — and the map
//! between its frames and the wall clock. That map is what makes gapless
//! playback and A/V synchronisation arithmetic instead of a guess, and it is
//! what turns drift between two devices into a reported number rather than a
//! mystery nobody can debug.
//!
//! # Untrusted by construction
//!
//! The pairs come from a driver process. A position that went backwards, a
//! timestamp that did, or a pair that implies a rate no converter runs at is
//! refused and left out of the fit: a broken or hostile driver degrades its
//! own device's reported rate to the nominal one, and poisons nothing.
//!
//! # It says when it does not know
//!
//! Below [`MIN_SAMPLES`] observations, or where the fit lands outside a
//! plausible band around the nominal rate, [`ClockModel::measured_millihertz`]
//! answers [`None`] and the nominal rate stands. A crystal is accurate to
//! parts per million; a fit that says otherwise is measuring something else.

use tairix_abi::audio::ClockReport;
use tairix_abi::driver::audio::{Frames, Rate};
use tairix_abi::time::Time64;
use tairix_abi::Errno;

/// Observations the fit is computed over.
///
/// A filter length rather than a capacity: at a five-millisecond period this
/// spans a third of a second, over which timestamp jitter averages down to a
/// few parts per million — and a longer window would track a real rate change
/// more slowly rather than measure the current one better.
pub const WINDOW: usize = 64;

/// Observations before a fit is offered at all.
pub const MIN_SAMPLES: usize = 8;

/// How far from nominal a fit may land and still be believed, in parts per
/// million.
///
/// A converter crystal is specified in tens of parts per million and an
/// adaptive USB clock in hundreds. Five per cent is three orders past any of
/// them, so a fit outside it is a measurement of something other than the
/// device's rate — a stalled driver, a clock that jumped — and the nominal
/// rate is the honest answer.
pub const PLAUSIBLE_PPM: u64 = 50_000;

/// One reported (position, time) pair.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Observation {
    position: u64,
    nanos: i128,
}

/// A device's exported clock: its nominal rate, its measured one, and the map
/// between its frames and the wall clock.
#[derive(Clone, Debug)]
pub struct ClockModel {
    nominal: Rate,
    /// The window, oldest first. Bounded, so the model's memory is a fixed
    /// few kilobytes per device rather than a function of how long it has run.
    samples: [Observation; WINDOW],
    count: usize,
    next: usize,
    /// The most recent accepted pair, which is what a report quotes.
    latest: Option<(Frames, Time64)>,
}

impl ClockModel {
    /// A model for a device whose nominal rate is `nominal`, with nothing
    /// observed yet.
    #[must_use]
    pub fn new(nominal: Rate) -> Self {
        Self {
            nominal,
            samples: [Observation {
                position: 0,
                nanos: 0,
            }; WINDOW],
            count: 0,
            next: 0,
            latest: None,
        }
    }

    /// Forget every observation — what a device reconfiguration or a
    /// re-attach leaves behind, since the old pairs describe a clock that is
    /// no longer running.
    pub fn reset(&mut self) {
        self.count = 0;
        self.next = 0;
        self.latest = None;
    }

    /// Record one reported pair.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] when the position or the time went backwards
    /// against the last accepted pair, or when the two together imply a rate
    /// no converter runs at. The pair is discarded rather than fitted.
    pub fn observe(&mut self, position: Frames, at: Time64) -> Result<(), Errno> {
        let nanos = time_nanos(at);
        if let Some((last_position, last_time)) = self.latest {
            let elapsed = nanos - time_nanos(last_time);
            let advanced = position.since(last_position).ok_or(Errno::OutOfRange)?;
            if elapsed < 0 {
                return Err(Errno::OutOfRange);
            }
            // A pair whose implied instantaneous rate is outside the
            // vocabulary is not a measurement of this device.
            if elapsed > 0 && !plausible_step(advanced, elapsed) {
                return Err(Errno::OutOfRange);
            }
        }
        self.samples[self.next] = Observation {
            position: position.get(),
            nanos,
        };
        self.next = (self.next + 1) % WINDOW;
        self.count = (self.count + 1).min(WINDOW);
        self.latest = Some((position, at));
        Ok(())
    }

    /// The most recent accepted pair.
    #[must_use]
    pub const fn latest(&self) -> Option<(Frames, Time64)> {
        self.latest
    }

    /// The measured rate in thousandths of a hertz, or [`None`] while the
    /// window is too short or the fit is implausible.
    #[must_use]
    pub fn measured_millihertz(&self) -> Option<u32> {
        if self.count < MIN_SAMPLES {
            return None;
        }
        let frames_per_second = self.slope()?;
        let millihertz = frames_per_second * 1_000.0;
        if !millihertz.is_finite() || millihertz <= 0.0 {
            return None;
        }
        let measured = u32::try_from(round_u64(millihertz)).ok()?;
        let nominal = self.nominal_millihertz();
        let drift = u64::from(measured.abs_diff(nominal));
        // Compared against the nominal in the same units, so the band is the
        // same width whatever the device is clocked at.
        if drift.checked_mul(1_000_000)? > u64::from(nominal).checked_mul(PLAUSIBLE_PPM)? {
            return None;
        }
        Some(measured)
    }

    /// The configured rate in thousandths of a hertz — the answer whenever
    /// the measurement is not yet trustworthy.
    #[must_use]
    pub fn nominal_millihertz(&self) -> u32 {
        self.nominal.hz().saturating_mul(1_000)
    }

    /// The rate to report: measured where the fit is believable, nominal
    /// otherwise.
    #[must_use]
    pub fn reported_millihertz(&self) -> u32 {
        self.measured_millihertz()
            .unwrap_or_else(|| self.nominal_millihertz())
    }

    /// The exported clock, or [`None`] before the device has reported
    /// anything at all.
    #[must_use]
    pub fn report(&self) -> Option<ClockReport> {
        let (position, sampled_at) = self.latest?;
        Some(ClockReport {
            position,
            rate_millihertz: self.reported_millihertz(),
            sampled_at,
        })
    }

    /// Where the device will be at `when`, by the fitted line.
    ///
    /// [`None`] before anything has been observed, or where the answer would
    /// be before the start of the stream.
    #[must_use]
    pub fn position_at(&self, when: Time64) -> Option<Frames> {
        let (position, sampled_at) = self.latest?;
        let elapsed = nanos_f64(time_nanos(when) - time_nanos(sampled_at));
        let advanced = elapsed * self.rate_f64() / 1e9;
        let target = position_f64(position.get()) + advanced;
        if !target.is_finite() || target < 0.0 {
            return None;
        }
        Some(Frames::new(round_u64(target)))
    }

    /// When the device reaches `position`, by the fitted line.
    ///
    /// [`None`] before anything has been observed, or where the answer falls
    /// outside a representable time.
    #[must_use]
    pub fn time_at(&self, position: Frames) -> Option<Time64> {
        let (last_position, sampled_at) = self.latest?;
        let ahead = position_f64(position.get()) - position_f64(last_position.get());
        let nanos = ahead * 1e9 / self.rate_f64();
        if !nanos.is_finite() {
            return None;
        }
        let total = time_nanos(sampled_at).checked_add(round_i128(nanos))?;
        time_from_nanos(total)
    }

    /// The rate the map is drawn with, in frames per second.
    fn rate_f64(&self) -> f64 {
        f64::from(self.reported_millihertz()) / 1_000.0
    }

    /// The least-squares slope of position against time, in frames per
    /// second, or [`None`] when every observation shares one instant.
    fn slope(&self) -> Option<f64> {
        let window = self.window();
        let first = *window.first()?;
        let mut mean_time = 0.0;
        let mut mean_position = 0.0;
        let span = f64::from(u32::try_from(window.len()).ok()?);
        for sample in &window {
            mean_time += nanos_f64(sample.nanos - first.nanos) / span;
            mean_position += position_f64(sample.position - first.position) / span;
        }
        let mut covariance = 0.0;
        let mut variance = 0.0;
        for sample in &window {
            let time = nanos_f64(sample.nanos - first.nanos) - mean_time;
            let advance = position_f64(sample.position - first.position) - mean_position;
            covariance += time * advance;
            variance += time * time;
        }
        if variance <= 0.0 {
            return None;
        }
        Some(covariance / variance * 1e9)
    }

    /// The observations, oldest first, as one contiguous scratch copy.
    ///
    /// Copied onto the stack rather than allocated: the window is a fixed few
    /// kilobytes and the fit runs off the per-period path.
    fn window(&self) -> [Observation; WINDOW] {
        let mut ordered = [Observation {
            position: 0,
            nanos: 0,
        }; WINDOW];
        let start = (self.next + WINDOW - self.count) % WINDOW;
        for (slot, target) in ordered.iter_mut().enumerate().take(self.count) {
            *target = self.samples[(start + slot) % WINDOW];
        }
        ordered
    }
}

/// Whether `advanced` frames in `elapsed` nanoseconds is a rate the
/// vocabulary admits, with a decade of slack either side so a single jittery
/// period is not thrown away.
fn plausible_step(advanced: u64, elapsed: i128) -> bool {
    let Ok(elapsed) = u128::try_from(elapsed) else {
        return false;
    };
    let rate = u128::from(advanced) * 1_000_000_000 / elapsed.max(1);
    rate <= u128::from(Rate::MAX_HZ) * 10
}

/// `time` as whole nanoseconds since the epoch.
fn time_nanos(time: Time64) -> i128 {
    i128::from(time.secs()) * 1_000_000_000 + i128::from(time.subsec_nanos())
}

/// A whole-nanosecond count back into a time, or [`None`] outside the range a
/// [`Time64`] holds.
fn time_from_nanos(total: i128) -> Option<Time64> {
    let secs = total.div_euclid(1_000_000_000);
    let nanos = total.rem_euclid(1_000_000_000);
    Time64::new(i64::try_from(secs).ok()?, u32::try_from(nanos).ok()?).ok()
}

/// A nanosecond span as a real number. Spans here are the window's, so they
/// are seconds rather than epochs and the conversion loses nothing that
/// matters to a slope.
#[allow(
    clippy::cast_precision_loss,
    reason = "the value is a difference within the fit's own window — under a \
              second in practice — where a double is exact to well past a \
              nanosecond"
)]
fn nanos_f64(nanos: i128) -> f64 {
    nanos as f64
}

/// A frame count as a real number.
#[allow(
    clippy::cast_precision_loss,
    reason = "positions are differences within the fit's window, and even an \
              absolute one stays exact in a double past any stream length a \
              machine will run"
)]
fn position_f64(position: u64) -> f64 {
    position as f64
}

/// `value` rounded to the nearest non-negative whole number, saturating.
#[allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    reason = "the value is clamped into the unsigned range on the line above, \
              so neither the sign loss nor the truncation the lints warn about \
              can occur"
)]
fn round_u64(value: f64) -> u64 {
    let rounded = tairix_util::mathf::clamp(
        tairix_util::mathf::floor(value + 0.5),
        0.0,
        9_007_199_254_740_992.0,
    );
    rounded as u64
}

/// `value` rounded to the nearest whole number of nanoseconds.
#[allow(
    clippy::cast_possible_truncation,
    reason = "the value is clamped into the range a double represents exactly \
              on the line above, which is far inside i128"
)]
fn round_i128(value: f64) -> i128 {
    let rounded = tairix_util::mathf::clamp(
        tairix_util::mathf::floor(value + 0.5),
        -9_007_199_254_740_992.0,
        9_007_199_254_740_992.0,
    );
    rounded as i128
}

#[cfg(test)]
#[path = "clock_tests.rs"]
mod tests;
