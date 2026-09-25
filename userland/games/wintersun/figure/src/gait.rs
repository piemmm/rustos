//! Gait phase from distance travelled, and the foot-slide it is fitted by.
//!
//! A walk cycle driven by a timer is a walk cycle that slides: change the
//! speed and the feet keep their old cadence, so they skate over the ground,
//! and a figure held still walks on the spot. Driving the phase from the
//! distance the figure has actually covered removes the failure by
//! construction — a foot is planted as a function of *position*, so it cannot
//! move while the ground under it does not.
//!
//! That leaves one number to get right: the stride, the distance one full
//! cycle carries the figure. Authored by hand it is a guess, and a guess is
//! what puts the slide back. [`Gait::fitted`] measures it instead, from the
//! clip and the rig themselves: it finds the span of the cycle where the foot
//! is on the ground and asks how far the foot travels backward through the
//! body over it, which is exactly how far the body must travel forward for
//! the foot to stay still. [`Gait::slide`] then reports what is left, so
//! "the feet do not skate" is a measured number rather than an opinion.
//!
//! Where the foot is on the ground is read over the ground, at the height the
//! clip holds the body: a stance that sinks the body while the leg folds into
//! it keeps the foot on the floor but raises it through the body frame, and a
//! contact window taken there would see only the ends of the step.

use tairix_util::mathf;

use crate::clip::Clip;
use crate::error::FigureError;
use crate::plant::Legs;
use crate::rig::{Frames, Resolved};
use crate::rigging::Rigging;
use crate::socket::Side;

/// How finely a cycle is sampled when a stride is measured.
///
/// The resolution of the measurement, not a caller's knob: it fixes the
/// precision of the contact window to under two percent of a cycle, which is
/// finer than the window's own edges are defined, and keeps the trace on the
/// stack so a fitting costs no allocator.
pub const SAMPLES: usize = 64;

/// How far above the foot's lowest point still counts as ground contact, as a
/// fraction of the foot's whole vertical travel over the cycle.
///
/// A window defined at the exact minimum would be one sample wide, since a
/// heel strikes before a toe and lifts after it; a window defined loosely
/// admits samples from the swing, where the foot is travelling forward, and
/// those drag the measured stride down towards nothing. A fiftieth of the
/// lift is wide enough for the flat of a step and far too narrow for any
/// part of the arc over it — measured against the shipped walk, every band
/// from a hundredth to a twentieth picks out the same window, so this sits
/// in the middle of a plateau rather than on an edge.
const CONTACT_BAND: f64 = 0.02;

/// `count` as a real.
///
/// Every count this module converts is a sample index or a run length, both
/// bounded by [`SAMPLES`], so each is exact in a double many times over.
#[allow(
    clippy::cast_precision_loss,
    reason = "bounded by SAMPLES, far below the mantissa's own range"
)]
fn real(count: usize) -> f64 {
    count as f64
}

/// A foot's path over one cycle: through the body across the ground, and
/// over the ground in height.
struct Trace {
    forward: [f64; SAMPLES],
    side: [f64; SAMPLES],
    up: [f64; SAMPLES],
    ceiling: f64,
}

impl Trace {
    /// Where `foot` of `legs` stands at each of [`SAMPLES`] phases of `clip`.
    fn of(
        rigging: &Rigging<'_>,
        clip: Clip<'_>,
        legs: &Legs,
        foot: Side,
    ) -> Result<Self, FigureError> {
        let mut trace = Self {
            forward: [0.0; SAMPLES],
            side: [0.0; SAMPLES],
            up: [0.0; SAMPLES],
            ceiling: 0.0,
        };
        let mut frames = Frames::new();
        for index in 0..SAMPLES {
            // The cycle is sampled half-open: phase one is phase zero again.
            let phase = real(index) / real(SAMPLES);
            let pose = clip.sample(phase)?;
            rigging.posture(&pose)?.resolve(Resolved::REST, &mut frames);
            let at = legs.standing(&frames, clip.root_at(phase))?[foot as usize];
            trace.forward[index] = at.forward;
            trace.side[index] = at.side;
            trace.up[index] = at.up;
        }

        let mut lowest = trace.up[0];
        let mut highest = trace.up[0];
        for height in trace.up {
            lowest = mathf::fmin(lowest, height);
            highest = mathf::fmax(highest, height);
        }
        trace.ceiling = lowest + (highest - lowest) * CONTACT_BAND;
        Ok(trace)
    }

    /// Whether the foot is on the ground at sample `index`, cyclically.
    fn down(&self, index: usize) -> bool {
        self.up[index % SAMPLES] <= self.ceiling
    }

    /// The start and length of the longest run of samples the foot is down
    /// for.
    ///
    /// Cyclic, so a foot planted across the join of the cycle is one window
    /// rather than two — which is what a clip authored to start mid-stance
    /// produces. The *longest* run rather than the first, so a stance with a
    /// brief blip in it measures over the real step instead of over whichever
    /// fragment happened to come first.
    fn contact(&self) -> Result<(usize, usize), FigureError> {
        // A window's first sample is one whose predecessor is airborne. A
        // foot down for the whole cycle has no such sample and is not a walk.
        let mut best = (0, 0);
        for start in (0..SAMPLES).filter(|&i| self.down(i) && !self.down(i + SAMPLES - 1)) {
            let length = (0..SAMPLES)
                .take_while(|&step| self.down(start + step))
                .count();
            if length > best.1 {
                best = (start, length);
            }
        }
        if best.1 == 0 {
            return Err(FigureError::StrideUnreal);
        }
        if best.1 < 2 {
            return Err(FigureError::SamplesTooFew);
        }
        Ok(best)
    }

    /// How fast the foot travels backward through the body over the window,
    /// per cycle.
    ///
    /// The least-squares slope rather than the difference of the two ends, so
    /// the answer rests on every sample of the stance instead of on exactly
    /// where the window's edges fell.
    fn backward_rate(&self, start: usize, length: usize) -> f64 {
        let count = real(length);
        let (mut mean_phase, mut mean_forward) = (0.0, 0.0);
        for step in 0..length {
            mean_phase += real(step) / real(SAMPLES);
            mean_forward += self.forward[(start + step) % SAMPLES];
        }
        mean_phase /= count;
        mean_forward /= count;
        let (mut covariance, mut spread) = (0.0, 0.0);
        for step in 0..length {
            let phase = real(step) / real(SAMPLES) - mean_phase;
            covariance += phase * (self.forward[(start + step) % SAMPLES] - mean_forward);
            spread += phase * phase;
        }
        // At least two distinct sample phases, so the spread is positive.
        -covariance / spread
    }
}

/// How far one full cycle of a walk carries the figure, and where in that
/// cycle it currently is.
///
/// The phase is the number a clip is sampled at, so a gait replaces
/// [`Clip::phase_at`] rather than sitting beside it: the same clip, driven by
/// where the figure is instead of by what time it is.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Gait {
    stride: f64,
    phase: f64,
}

/// What a cycle crossed while the figure moved.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Stepped {
    /// The phase before the move.
    pub from: f64,
    /// The phase after it.
    pub to: f64,
    /// Whole cycles completed, so a step long enough to skip one is known to
    /// have skipped it rather than losing its events silently.
    pub cycles: u32,
}

impl Gait {
    /// A gait of `stride` distance per cycle, starting at the head of it.
    ///
    /// # Errors
    ///
    /// [`FigureError::StrideUnreal`] for a stride that is not finite and
    /// positive.
    pub fn new(stride: f64) -> Result<Self, FigureError> {
        if !stride.is_finite() || stride <= 0.0 {
            return Err(FigureError::StrideUnreal);
        }
        Ok(Self { stride, phase: 0.0 })
    }

    /// The gait whose stride leaves `clip`'s planted `side` foot of `legs`
    /// standing still.
    ///
    /// Measured rather than authored: the foot's backward travel through the
    /// body while it is on the ground, over the fraction of the cycle it is
    /// down for, is the speed the body must move at for it not to slide.
    ///
    /// # Errors
    ///
    /// [`FigureError::NoSuchJoint`] if the resolve does not cover the leg,
    /// [`FigureError::SamplesTooFew`] for a contact window too short to
    /// measure a span over, and [`FigureError::StrideUnreal`] for a clip
    /// whose foot never leaves the ground or never travels backward through
    /// the body — neither of which is a walk.
    pub fn fitted(
        rigging: &Rigging<'_>,
        clip: Clip<'_>,
        legs: &Legs,
        side: Side,
    ) -> Result<Self, FigureError> {
        let trace = Trace::of(rigging, clip, legs, side)?;
        let (start, length) = trace.contact()?;
        Self::new(trace.backward_rate(start, length))
    }

    /// The furthest the planted foot still moves over the ground, per cycle.
    ///
    /// Zero for a clip whose foot travels backward through the body at
    /// exactly this gait's rate throughout its stance; otherwise the size of
    /// the skate, in the same figure-local units the rig is authored in. What
    /// makes "the feet do not slide" a number a test can bound.
    ///
    /// # Errors
    ///
    /// As [`Self::fitted`].
    pub fn slide(
        self,
        rigging: &Rigging<'_>,
        clip: Clip<'_>,
        legs: &Legs,
        side: Side,
    ) -> Result<f64, FigureError> {
        let trace = Trace::of(rigging, clip, legs, side)?;
        let (start, length) = trace.contact()?;
        let (mut least, mut most) = (f64::MAX, f64::MIN);
        let (mut narrowest, mut widest) = (f64::MAX, f64::MIN);
        for step in 0..length {
            let index = (start + step) % SAMPLES;
            // Where the foot is over the ground: how far the body has carried
            // it, plus where it sits in the body.
            let over = self.stride * (real(step) / real(SAMPLES)) + trace.forward[index];
            least = mathf::fmin(least, over);
            most = mathf::fmax(most, over);
            narrowest = mathf::fmin(narrowest, trace.side[index]);
            widest = mathf::fmax(widest, trace.side[index]);
        }
        Ok(mathf::hypot(most - least, widest - narrowest))
    }

    /// How far one cycle carries the figure.
    #[must_use]
    pub const fn stride(self) -> f64 {
        self.stride
    }

    /// The same cycle at the same phase, paced by `stride` from here on.
    ///
    /// A blend of two gaits keeps one phase — the walk's left foot is the
    /// run's left foot — and paces it by the blend's own stride, so the
    /// feet stay in step while the blend moves between them.
    ///
    /// # Errors
    ///
    /// [`FigureError::StrideUnreal`] for a stride that is not finite and
    /// positive.
    pub fn restrided(self, stride: f64) -> Result<Self, FigureError> {
        Ok(Self {
            phase: self.phase,
            ..Self::new(stride)?
        })
    }

    /// Where in the cycle it is, in `0..1`.
    #[must_use]
    pub const fn phase(self) -> f64 {
        self.phase
    }

    /// Put it at `phase` of the cycle.
    ///
    /// What a landing or a standing start uses: a figure that stops and
    /// starts again should plant its next foot from a known place rather than
    /// from wherever the last one left off.
    ///
    /// # Errors
    ///
    /// [`FigureError::PhaseOutsideClip`] for a phase outside `0..=1`.
    pub fn set_phase(&mut self, phase: f64) -> Result<(), FigureError> {
        if !phase.is_finite() || !(0.0..=1.0).contains(&phase) {
            return Err(FigureError::PhaseOutsideClip);
        }
        self.phase = wrap(phase);
        Ok(())
    }

    /// Advance the cycle by `distance` travelled.
    ///
    /// Signed: walking backward runs the cycle backward, so the feet still do
    /// not slide. The phases it reports bracket the move and feed
    /// [`Clip::events_between`] directly, which is what makes a footstep fire
    /// on the frame the foot lands rather than on a timer beside it.
    ///
    /// # Errors
    ///
    /// [`FigureError::DistanceUnreal`] for a distance that is not finite.
    pub fn travel(&mut self, distance: f64) -> Result<Stepped, FigureError> {
        if !distance.is_finite() {
            return Err(FigureError::DistanceUnreal);
        }
        let from = self.phase;
        let advanced = from + distance / self.stride;
        self.phase = wrap(advanced);
        Ok(Stepped {
            from,
            to: self.phase,
            cycles: crossings(advanced),
        })
    }
}

/// `phase` brought into `0..1`.
///
/// A phase of exactly one is the head of the next cycle, not the tail of this
/// one, so the window a consumer steps over never repeats a sample.
fn wrap(phase: f64) -> f64 {
    let wrapped = phase - mathf::floor(phase);
    // `floor` of a value a hair below an integer can round the difference up
    // to exactly one, which would leave the phase outside the half-open
    // cycle it must be in.
    if wrapped >= 1.0 {
        0.0
    } else {
        wrapped
    }
}

/// How many cycle boundaries a phase of `advanced` crossed from `0..1`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the value is bracketed into u32's range on the lines above, so \
              neither the truncation nor the sign loss the lints warn about \
              can occur"
)]
fn crossings(advanced: f64) -> u32 {
    let crossed = mathf::fabs(mathf::floor(advanced));
    if crossed >= f64::from(u32::MAX) {
        return u32::MAX;
    }
    crossed as u32
}

#[cfg(test)]
#[path = "gait/tests.rs"]
mod tests;
