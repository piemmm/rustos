//! Clips: a keyed curve per parameter, the events at phases along it, and how
//! a phase advances with time.
//!
//! A clip is keyed against a *phase* in `0..=1` rather than against seconds,
//! so the same clip can be retimed by changing one number, and so a walk can
//! later be driven by distance travelled instead of by a clock without the
//! curves knowing the difference.
//!
//! No easing overshoots. That is deliberate and load-bearing: every value a
//! curve produces lies between the two keys it sits between, so a clip
//! authored inside its parameters' ranges cannot leave them, and a pose can
//! never reach a joint limit it was not allowed to. Overshoot — the snap of a
//! recoil, the settle of a follow-through — is a damped layer over the clip,
//! where it can be bounded on its own terms.

use tairix_util::mathf;

use crate::error::FigureError;
use crate::pose::{Mask, Param, Pose};

/// How the segment starting at a key reaches the next one.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Easing {
    /// Constant rate.
    Linear,
    /// Slow to start.
    EaseIn,
    /// Slow to finish.
    EaseOut,
    /// Slow at both ends.
    EaseInOut,
    /// No interpolation: the value steps at the next key.
    Hold,
}

impl Easing {
    /// The eased fraction for a linear fraction `t` of the segment.
    ///
    /// Maps `0..=1` onto `0..=1` and never outside, which is what keeps an
    /// interpolated value between its two keys.
    #[must_use]
    pub fn apply(self, t: f64) -> f64 {
        match self {
            Self::Linear => t,
            Self::EaseIn => t * t,
            Self::EaseOut => t * (2.0 - t),
            Self::EaseInOut => t * t * (3.0 - 2.0 * t),
            Self::Hold => 0.0,
        }
    }
}

/// One keyed value on a curve.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Key {
    /// Where along the clip it sits, in `0..=1`.
    pub phase: f64,
    /// What the parameter is there.
    pub value: f64,
    /// How the segment *starting* here reaches the next key.
    pub easing: Easing,
}

impl Key {
    /// A key at `phase` holding `value`, easing linearly toward the next.
    #[must_use]
    pub const fn new(phase: f64, value: f64) -> Self {
        Self {
            phase,
            value,
            easing: Easing::Linear,
        }
    }

    /// The same key, easing toward the next key by `easing`.
    #[must_use]
    pub const fn eased(mut self, easing: Easing) -> Self {
        self.easing = easing;
        self
    }
}

/// How a clip's phase behaves past its end.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Loop {
    /// Play once and hold the last pose.
    Hold,
    /// Cycle: the end joins the start, and the last key eases into the first.
    Wrap,
    /// Play forward, then backward, without end.
    PingPong,
}

impl Loop {
    /// Whether the last key interpolates round to the first.
    #[must_use]
    const fn joins(self) -> bool {
        matches!(self, Self::Wrap)
    }
}

/// A named moment at a phase of a clip.
///
/// The seam the game's timing is built on: a hitbox opens, a sound plays, or
/// an arrow looses on the frame the art shows it rather than on a timer that
/// drifts from it. The engine carries the name and the phase and learns
/// nothing about what either means.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Event {
    /// What the consumer knows it by.
    pub name: &'static str,
    /// Where along the clip it happens, in `0..=1`.
    pub phase: f64,
}

impl Event {
    /// An event called `name` at `phase`.
    #[must_use]
    pub const fn new(name: &'static str, phase: f64) -> Self {
        Self { name, phase }
    }
}

/// One parameter's keyed curve.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Curve<'a> {
    param: Param,
    keys: &'a [Key],
}

impl<'a> Curve<'a> {
    /// A curve on `param` through `keys`.
    ///
    /// # Errors
    ///
    /// [`FigureError::CurveEmpty`] for no keys,
    /// [`FigureError::KeysNotAscending`] for keys that do not strictly
    /// ascend by phase, [`FigureError::PhaseOutsideClip`] for a phase outside
    /// `0..=1`, and [`FigureError::ParamOutsideRange`] for a keyed value
    /// outside what the parameter is authored in — which is where a clip that
    /// would drive a joint past its travel fails.
    pub fn new(param: Param, keys: &'a [Key]) -> Result<Self, FigureError> {
        let Some(first) = keys.first() else {
            return Err(FigureError::CurveEmpty);
        };
        if !(0.0..=1.0).contains(&first.phase) {
            return Err(FigureError::PhaseOutsideClip);
        }
        let range = param.range();
        if !range.holds(first.value) {
            return Err(FigureError::ParamOutsideRange);
        }
        for pair in keys.windows(2) {
            let (previous, key) = (pair[0], pair[1]);
            if !(0.0..=1.0).contains(&key.phase) {
                return Err(FigureError::PhaseOutsideClip);
            }
            if key.phase <= previous.phase {
                return Err(FigureError::KeysNotAscending);
            }
            if !range.holds(key.value) {
                return Err(FigureError::ParamOutsideRange);
            }
        }
        Ok(Self { param, keys })
    }

    /// The parameter it drives.
    #[must_use]
    pub const fn param(self) -> Param {
        self.param
    }

    /// Its keys, ascending by phase.
    #[must_use]
    pub const fn keys(self) -> &'a [Key] {
        self.keys
    }

    /// Its value at `phase`, under `repeat`.
    ///
    /// Always between the two keys it lies between, so always inside the
    /// parameter's range.
    #[must_use]
    pub fn sample(self, phase: f64, repeat: Loop) -> f64 {
        let keys = self.keys;
        let (Some(first), Some(last)) = (keys.first(), keys.last()) else {
            return 0.0;
        };
        if keys.len() == 1 {
            return first.value;
        }

        let upper = keys.partition_point(|key| key.phase <= phase);
        let value = if upper == 0 {
            if repeat.joins() {
                between(*last, *first, last.phase - 1.0, first.phase, phase)
            } else {
                first.value
            }
        } else if upper == keys.len() {
            if repeat.joins() {
                between(*last, *first, last.phase, first.phase + 1.0, phase)
            } else {
                last.value
            }
        } else {
            let (from, to) = (keys[upper - 1], keys[upper]);
            between(from, to, from.phase, to.phase, phase)
        };
        self.param.range().clamp(value)
    }
}

/// `from`'s value eased toward `to`'s, at `phase` across `start..=end`.
fn between(from: Key, to: Key, start: f64, end: f64, phase: f64) -> f64 {
    let span = end - start;
    if span <= 0.0 {
        return from.value;
    }
    let t = mathf::clamp((phase - start) / span, 0.0, 1.0);
    from.value + (to.value - from.value) * from.easing.apply(t)
}

/// A keyed animation: curves, events, a duration and a loop mode.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Clip<'a> {
    curves: &'a [Curve<'a>],
    events: &'a [Event],
    seconds: f64,
    repeat: Loop,
    mask: Mask,
}

impl<'a> Clip<'a> {
    /// A clip of `seconds` looping by `repeat`, over `curves`, firing
    /// `events`.
    ///
    /// # Errors
    ///
    /// [`FigureError::DurationUnreal`] for a duration that is not finite and
    /// positive, [`FigureError::DuplicateCurve`] for two curves on one
    /// parameter, [`FigureError::PhaseOutsideClip`] for an event outside
    /// `0..=1`, [`FigureError::EventsNotAscending`] for events that do not
    /// ascend by phase, and [`FigureError::DuplicateEventName`] for one name
    /// used twice.
    pub fn new(
        seconds: f64,
        repeat: Loop,
        curves: &'a [Curve<'a>],
        events: &'a [Event],
    ) -> Result<Self, FigureError> {
        if !seconds.is_finite() || seconds <= 0.0 {
            return Err(FigureError::DurationUnreal);
        }

        let mut mask = Mask::NONE;
        for curve in curves {
            if mask.holds(curve.param()) {
                return Err(FigureError::DuplicateCurve);
            }
            mask = mask.with(curve.param());
        }

        for (index, event) in events.iter().enumerate() {
            if !event.phase.is_finite() || !(0.0..=1.0).contains(&event.phase) {
                return Err(FigureError::PhaseOutsideClip);
            }
            if index > 0 && event.phase < events[index - 1].phase {
                return Err(FigureError::EventsNotAscending);
            }
            if events[..index].iter().any(|prior| prior.name == event.name) {
                return Err(FigureError::DuplicateEventName);
            }
        }

        Ok(Self {
            curves,
            events,
            seconds,
            repeat,
            mask,
        })
    }

    /// How long one play of it lasts, in seconds.
    #[must_use]
    pub const fn seconds(self) -> f64 {
        self.seconds
    }

    /// How its phase behaves past the end.
    #[must_use]
    pub const fn repeat(self) -> Loop {
        self.repeat
    }

    /// The parameters it writes.
    #[must_use]
    pub const fn mask(self) -> Mask {
        self.mask
    }

    /// Its curves.
    #[must_use]
    pub const fn curves(self) -> &'a [Curve<'a>] {
        self.curves
    }

    /// Its events, ascending by phase.
    #[must_use]
    pub const fn events(self) -> &'a [Event] {
        self.events
    }

    /// The phase `elapsed` seconds into it, under its loop mode.
    ///
    /// # Errors
    ///
    /// [`FigureError::ElapsedUnreal`] for an elapsed time that is not finite.
    pub fn phase_at(self, elapsed: f64) -> Result<f64, FigureError> {
        if !elapsed.is_finite() {
            return Err(FigureError::ElapsedUnreal);
        }
        let plays = elapsed / self.seconds;
        Ok(match self.repeat {
            Loop::Hold => mathf::clamp(plays, 0.0, 1.0),
            Loop::Wrap => {
                let phase = plays - mathf::floor(plays);
                mathf::clamp(phase, 0.0, 1.0)
            }
            Loop::PingPong => {
                let cycle = plays * 0.5;
                let swung = (cycle - mathf::floor(cycle)) * 2.0;
                let phase = if swung <= 1.0 { swung } else { 2.0 - swung };
                mathf::clamp(phase, 0.0, 1.0)
            }
        })
    }

    /// Whole plays of it crossed going from `before` to `after` seconds.
    ///
    /// What a consumer needs to know that a step longer than the clip skipped
    /// a cycle's events rather than losing them silently. A held clip never
    /// repeats and so crosses none, and a ping-pong's cycle is there *and
    /// back*, so it is twice the duration.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the value is bracketed into u32's range on the lines above, \
                  so neither the truncation nor the sign loss the lints warn \
                  about can occur"
    )]
    #[must_use]
    pub fn laps_between(self, before: f64, after: f64) -> u32 {
        let cycle = match self.repeat {
            Loop::Hold => return 0,
            Loop::Wrap => self.seconds,
            Loop::PingPong => self.seconds * 2.0,
        };
        if !before.is_finite() || !after.is_finite() {
            return 0;
        }
        let crossed = mathf::floor(after / cycle) - mathf::floor(before / cycle);
        if crossed <= 0.0 {
            return 0;
        }
        if crossed >= f64::from(u32::MAX) {
            return u32::MAX;
        }
        crossed as u32
    }

    /// Write its pose at `phase` into `pose`, leaving unkeyed parameters
    /// alone.
    ///
    /// # Errors
    ///
    /// [`FigureError::PhaseOutsideClip`] for a phase outside `0..=1`. A
    /// sampled value cannot be out of range, so the write itself cannot fail.
    pub fn sample_into(self, phase: f64, pose: &mut Pose) -> Result<(), FigureError> {
        if !phase.is_finite() || !(0.0..=1.0).contains(&phase) {
            return Err(FigureError::PhaseOutsideClip);
        }
        for curve in self.curves {
            pose.set(curve.param(), curve.sample(phase, self.repeat))?;
        }
        Ok(())
    }

    /// Its pose at `phase`, with every unkeyed parameter at rest.
    ///
    /// # Errors
    ///
    /// As [`Self::sample_into`].
    pub fn sample(self, phase: f64) -> Result<Pose, FigureError> {
        let mut pose = Pose::REST;
        self.sample_into(phase, &mut pose)?;
        Ok(pose)
    }

    /// The events crossed advancing the phase from `from` to `to`.
    ///
    /// Half-open — an event exactly at `to` fires, one exactly at `from` does
    /// not — so advancing across a phase fires its events once and never
    /// twice. `to` below `from` is a lap boundary and yields the tail of the
    /// clip before the head of the next, in the order they happen.
    ///
    /// # Errors
    ///
    /// [`FigureError::PhaseOutsideClip`] for a phase outside `0..=1`.
    pub fn events_between(
        self,
        from: f64,
        to: f64,
    ) -> Result<impl Iterator<Item = Event> + 'a, FigureError> {
        let inside = |phase: f64| phase.is_finite() && (0.0..=1.0).contains(&phase);
        if !inside(from) || !inside(to) {
            return Err(FigureError::PhaseOutsideClip);
        }
        let events = self.events;
        let lapped = to < from;
        let head_end = if lapped { 1.0 } else { to };
        let head = events
            .iter()
            .copied()
            .filter(move |event| event.phase > from && event.phase <= head_end);
        let tail = events
            .iter()
            .copied()
            .filter(move |event| lapped && event.phase <= to);
        Ok(head.chain(tail))
    }
}

#[cfg(test)]
#[path = "clip/tests.rs"]
mod tests;
