//! What a clip measures, so "the animation is good" is a number a test holds
//! rather than something a reviewer squints at.
//!
//! Each function answers a single figure, and each figure has a bound beside
//! it. Most bounds are the shipped art's, arrived at by measuring it and
//! leaving headroom; a few state the figure the art has to earn instead, and
//! say so where they do. Either way, widening one to admit a clip that fails
//! is the defect they exist to catch, not a fix.
//!
//! These live here rather than in the harness that renders the sheets for
//! three reasons: they are allocation-free statements about this crate's own
//! output, so `cargo test` runs them on every Tier-1 target rather than only
//! on the host; the harness and the crate's own tests then share one
//! definition of both the measurement and its bound; and a later figure
//! preset is measured by exactly the code the shipped one was.

use tairix_util::mathf;

use crate::clip::{Clip, Lift, Loop};
use crate::error::FigureError;
use crate::gait::{Gait, SAMPLES};
use crate::joint::{JointId, Limit};
use crate::plant::Legs;
use crate::pose::Range;
use crate::rig::{Frames, Posture, Resolved};
use crate::rigging::Rigging;
use crate::socket::Side;

/// The most of any joint's own travel a shipped clip may turn through.
///
/// Below one on purpose. A clip pinned at a limit reads as a rig fighting
/// itself, and leaves the planting and overlay layers above it nowhere to
/// go — every delta they add is clamped away. The shipped set's worst is the
/// walk's ankle at a little under four fifths.
pub const MAX_LIMIT_USE: f64 = 0.90;

/// The largest second difference any parameter, or the height the body is
/// held at, may show across a cycle, per unit of the range it is authored
/// in.
///
/// A pop is a discontinuity, and a discontinuity of any size at all shows up
/// here as a second difference of that size; a keyed curve sampled on this
/// grid shows only the faceting of its own key spacing. The bound sits
/// between the two — the shipped set's worst is a shade over four
/// hundredths, and a pop worth the name is several tenths — so it separates
/// a kink from a curve rather than measuring how finely the curve was
/// keyed.
pub const MAX_CONTINUITY: f64 = 0.06;

/// How far a looping clip's last pose may sit from its first, per unit of
/// the parameter's range.
///
/// Tight, because a cycle that does not join hitches on every lap and the
/// shipped tables are authored to join exactly.
pub const MAX_CLOSURE: f64 = 1e-9;

/// How far a planted foot may travel over the ground in one cycle, as a
/// fraction of the stride.
///
/// The skate. Zero for a clip whose foot travels backward through the body
/// at exactly the gait's own rate throughout its stance; what is left is the
/// slide a player reads as the feet not gripping.
///
/// A hundredth of a stride, which is the figure this plan states the fitting
/// has to earn rather than a measurement with headroom added: at the largest
/// size the desktop draws a figure it is about one pixel over a whole cycle,
/// so it is the point below which a slide stops being visible at all. The
/// shipped set's worst is the walk's, at 0.0027.
pub const MAX_SKATE: f64 = 0.01;

/// How far a clip's stated root height may leave its planted foot from the
/// ground, in figure-local units.
///
/// The clip authors the height its body is at and the leg curves author
/// where the feet are; nothing in the planting solve makes the two agree, so
/// a clip standing upright while folding its legs would sink its feet
/// through the floor and one crouching with straight legs would hover. This
/// is that disagreement, measured over every sample the foot is actually
/// down for.
///
/// The residue is the sag between two keys: a leg's angles are interpolated
/// linearly, and the foot they put down follows a slight arc below the line
/// the path holds it to, deepest halfway between keys — so the bound is the
/// key spacing's own sag rather than a judgement about art, and six-place
/// rounding adds under a hundred-thousandth of a unit to it. It scales with
/// the leg and with how fast the leg folds: the run's is 0.061 on the
/// hundred-unit reference human, 0.067 on the long-legged elf and 0.073 on
/// the tallest, longest-legged elf a record can describe, all well inside a
/// pixel at every size the desktop draws one.
pub const MAX_GROUNDING: f64 = 0.08;

/// The worst fraction of a joint's own travel any pose of `clip` uses.
///
/// Verified rather than computed from the drive table: every sampled pose is
/// turned into a [`Posture`], which refuses a rotation outside its joint's
/// documented limit, and the excursion is read back off the rotations the
/// posture actually holds. A drive whose sense or scaling was wrong fails
/// here rather than being reported as a comfortable number.
///
/// # Errors
///
/// [`FigureError::PhaseOutsideClip`] never, for a grid inside the cycle;
/// [`FigureError::RotationOutsideLimit`] where a clip drives a joint past
/// its travel, and [`FigureError::NoSuchJoint`] for a rig larger than a
/// joint index can name.
pub fn limits(rigging: &Rigging<'_>, clip: Clip<'_>) -> Result<f64, FigureError> {
    let mut worst = 0.0;
    for index in 0..SAMPLES {
        let pose = clip.sample(grid(index))?;
        let posture = rigging.posture(&pose)?;
        worst = mathf::fmax(worst, excursion(&posture)?);
    }
    Ok(worst)
}

/// The largest second difference of any parameter across `clip`, or of the
/// height it holds the body at, per unit of the range each is authored in.
///
/// Cyclic for a clip that joins, so a velocity break at the loop's own seam
/// is measured like any other; open-ended otherwise, where there is no seam
/// to cross.
#[must_use]
pub fn continuity(clip: Clip<'_>) -> f64 {
    let joined = clip.repeat() == Loop::Wrap;
    let mut worst = bend(|phase| clip.root_at(phase), Lift::RANGE, joined);
    for curve in clip.curves() {
        let sampled = bend(
            |phase| curve.sample(phase, clip.repeat()),
            curve.param().range(),
            joined,
        );
        worst = mathf::fmax(worst, sampled);
    }
    worst
}

/// The largest second difference `value` shows across the grid, per unit of
/// `range`.
fn bend(value: impl Fn(f64) -> f64, range: Range, joined: bool) -> f64 {
    let mut values = [0.0; SAMPLES];
    for (index, slot) in values.iter_mut().enumerate() {
        *slot = value(grid(index));
    }
    let span = span(range);
    let mut worst = 0.0;
    for index in 0..SAMPLES {
        if !joined && (index == 0 || index + 1 == SAMPLES) {
            continue;
        }
        let before = values[(index + SAMPLES - 1) % SAMPLES];
        let after = values[(index + 1) % SAMPLES];
        worst = mathf::fmax(
            worst,
            mathf::fabs(after - 2.0 * values[index] + before) / span,
        );
    }
    worst
}

/// How far `clip`'s last pose sits from its first, worst parameter, per unit
/// of that parameter's range.
#[must_use]
pub fn closure(clip: Clip<'_>) -> f64 {
    let mut worst = 0.0;
    for curve in clip.curves() {
        let gap = curve.sample(1.0, clip.repeat()) - curve.sample(0.0, clip.repeat());
        worst = mathf::fmax(worst, mathf::fabs(gap) / span(curve.param().range()));
    }
    worst
}

/// How far `clip`'s planted `side` foot of `legs` travels over the ground in
/// one cycle, as a fraction of the stride it was fitted to.
///
/// The gait already measures both halves of this; dividing them is what
/// turns a length in figure-local units into the dimensionless number a
/// bound can be stated about, whatever size the figure is drawn at.
///
/// # Errors
///
/// As [`Gait::fitted`] — in particular a clip whose foot never leaves the
/// ground has no gait to measure and is refused rather than given one.
pub fn skate(
    rigging: &Rigging<'_>,
    clip: Clip<'_>,
    legs: &Legs,
    side: Side,
) -> Result<f64, FigureError> {
    let gait = Gait::fitted(rigging, clip, legs, side)?;
    Ok(gait.slide(rigging, clip, legs, side)? / gait.stride())
}

/// How far the lowest point of `clip`'s own cycle sits from the floor, in
/// figure-local units.
///
/// The one check that the two halves of a grounded pose agree. The leg
/// curves say where the ankles are and the root height says where the body
/// is; over a whole cycle the lowest either foot ever reaches has to be the
/// floor exactly. Below it the figure's foot sinks into the ground, above it
/// the figure never touches down — the two are the same quantity's two
/// signs, so one number answers both.
///
/// Deliberately free of any notion of which foot is "down": a contact band
/// widens near a foot's lowest point, where its height is flat, so a window
/// drawn with one reports the foot planted well into its own toe-off. The
/// extreme over the cycle needs no window at all.
///
/// # Errors
///
/// [`FigureError::PhaseOutsideClip`] never, for a grid inside the cycle;
/// otherwise as the posture and the resolve, for a rig missing a leg.
pub fn grounding(rigging: &Rigging<'_>, clip: Clip<'_>, legs: &Legs) -> Result<f64, FigureError> {
    let mut frames = Frames::new();
    let mut deepest = f64::MAX;
    for index in 0..SAMPLES {
        let phase = grid(index);
        let pose = clip.sample(phase)?;
        rigging.posture(&pose)?.resolve(Resolved::REST, &mut frames);
        let standing = legs.standing(&frames, clip.root_at(phase))?;
        deepest = mathf::fmin(deepest, mathf::fmin(standing[0].up, standing[1].up));
    }
    Ok(mathf::fabs(deepest - legs.sole()))
}

/// The phase of sample `index` on the measurement grid.
///
/// Half-open, as a cycle is: the sample at phase one is the next cycle's
/// first, so it is never taken twice.
fn grid(index: usize) -> f64 {
    // Bounded by `SAMPLES`, far below the mantissa's own range.
    #[allow(clippy::cast_precision_loss, reason = "bounded by SAMPLES")]
    let fraction = index as f64 / SAMPLES as f64;
    fraction
}

/// How wide a parameter's authored interval is.
fn span(range: Range) -> f64 {
    range.max() - range.min()
}

/// The worst fraction of a joint's travel this posture turns through.
fn excursion(posture: &Posture<'_>) -> Result<f64, FigureError> {
    let mut worst = 0.0;
    for (index, joint) in posture.rig().joints().iter().enumerate() {
        let id = JointId::new(u8::try_from(index).map_err(|_| FigureError::NoSuchJoint)?);
        let turned = posture.get(id).ok_or(FigureError::NoSuchJoint)?;
        let limits = joint.limits;
        for (angle, limit) in [
            (turned.pitch, limits.pitch),
            (turned.yaw, limits.yaw),
            (turned.roll, limits.roll),
        ] {
            worst = mathf::fmax(worst, toward(angle, limit));
        }
    }
    Ok(worst)
}

/// How much of `limit`'s travel toward `angle`'s own side of rest it uses.
///
/// An axis that does not turn has no travel to use, and a posture cannot
/// have turned it, so it contributes nothing rather than dividing by zero.
fn toward(angle: f64, limit: Limit) -> f64 {
    let bound = if angle >= 0.0 {
        limit.max()
    } else {
        -limit.min()
    };
    if bound <= 0.0 {
        0.0
    } else {
        mathf::fabs(angle) / bound
    }
}

#[cfg(test)]
#[path = "quality/tests.rs"]
mod tests;
