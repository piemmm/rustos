//! Putting each foot on its own ground.
//!
//! The world generator makes real slopes, so a figure standing across a
//! gradient has one foot higher than the other. Drawn from the root's own
//! height both feet sit at the same level, and the camera looks straight down
//! at the line where they meet the ground — so the uphill foot visibly floats
//! and the downhill one sinks into the hill. This is a correctness
//! requirement rather than polish: the error is worst exactly where the eye
//! is already looking.
//!
//! # What the solve does
//!
//! A leg cannot stretch, so the lowest ground is the constraint: the hips come
//! down until that leg can reach it, and every other leg then takes up the
//! difference by bending. Each foot keeps the *plan* position the animation
//! gave it and changes only its height — the solve re-aims the hip and
//! re-folds the knee to put the ankle there, and the ankle turns back by as
//! much as the leg turned so the foot keeps the angle it was animated at.
//!
//! # The height the body is at is the clip's, not something read off the fold
//!
//! Both legs folded is a deep crouch and a run's flight phase at once, so no
//! reading of the articulation can tell the two apart. Inferring the height
//! from the *lesser* fold gets a stance right and a flight exactly wrong —
//! it sinks the figure by the tuck at the moment it should be rising. The
//! clip therefore states its own root height ([`Clip::root_at`]) and this
//! module only answers for the ground.
//!
//! So a foot is asked for the height the clip put it at, raised by the
//! terrain beneath it: the swing foot keeps its arc, the planted foot lands,
//! and neither needs to be picked out from the other. On flat ground every
//! target is exactly where the clip already had it, so the articulation
//! comes back untouched for *any* pose and the root is the authored one —
//! a figure on the level is drawn precisely as its clip authored it.
//!
//! Nothing here checks that a clip's stated height agrees with its own leg
//! keys; a clip claiming to stand upright while folding its legs would put
//! its feet through the floor. That agreement is a measured, bounded
//! property of the shipped set instead (`figure::quality`'s grounding), so
//! it is stated as a number rather than assumed by a solve.
//!
//! [`Clip::root_at`]: crate::clip::Clip::root_at
//!
//! # When the legs run out
//!
//! The height difference the legs can absorb is the span between a straight
//! leg and a fully folded one, which the rig states rather than this module.
//! Past it the whole figure tilts about its ground contact — a lean into the
//! hill rather than a rig torn apart — and what even that cannot reach is
//! *reported* as a miss rather than quietly fudged, because a figure standing
//! somewhere no figure could stand is the simulation's defect to see.

use tairix_util::mathf;

use crate::error::FigureError;
use crate::frame::{Basis, Body, Rotation};
use crate::joint::JointId;
use crate::pose::{Param, Pose};
use crate::rig::{Frames, Resolved};
use crate::rigging::Rigging;
use crate::socket::Side;

/// A leg of a rig, named by the joints it bends at and the parameters that
/// bend them.
///
/// Declared by whoever owns the rig rather than discovered here, so this
/// module knows what a two-bone chain is without knowing what a humanoid is.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Leg {
    /// Where the leg swings from.
    pub hip: JointId,
    /// Where it folds.
    pub knee: JointId,
    /// Where the foot hangs.
    pub ankle: JointId,
    /// Fore-and-aft swing of the hip.
    pub swing: Param,
    /// Sideways splay of the hip.
    pub splay: Param,
    /// How far the knee is folded.
    pub bend: Param,
    /// Flex of the ankle.
    pub flex: Param,
}

/// A pair of legs, measured against the rig they belong to.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Legs {
    sides: [Leg; 2],
    thigh: [f64; 2],
    shank: [f64; 2],
    hip: [Body; 2],
    sole: f64,
    reach: f64,
    straight: f64,
}

/// What a planting solve produced.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Planted {
    root: Resolved,
    pose: Pose,
    miss: [f64; 2],
}

impl Planted {
    /// The root transform the figure now stands in: how far it dropped, and
    /// how far it leaned.
    #[must_use]
    pub const fn root(&self) -> Resolved {
        self.root
    }

    /// The pose with the legs solved, ready for a posture.
    #[must_use]
    pub const fn pose(&self) -> Pose {
        self.pose
    }

    /// How far each foot ended up above the ground it was asked for.
    ///
    /// Zero whenever the ground was inside the legs' reach. A non-zero miss
    /// is the honest report that the figure was put somewhere it cannot
    /// stand, never a silently fudged foot.
    #[must_use]
    pub fn miss(&self, side: Side) -> f64 {
        self.miss[side as usize]
    }

    /// The furthest either foot is from the ground it was asked for.
    #[must_use]
    pub fn worst_miss(&self) -> f64 {
        mathf::fmax(mathf::fabs(self.miss[0]), mathf::fabs(self.miss[1]))
    }
}

impl Legs {
    /// Measure `legs` against `rig`.
    ///
    /// Bone lengths, the stance width and the height a sole rests at all come
    /// from the rig's own joint table, so a taller or wider figure needs no
    /// second set of numbers anywhere. The two legs are taken to reach the
    /// same ground, which is what a pair of legs is, so the height a sole
    /// rests at is the first one's straight-leg reach.
    ///
    /// # Errors
    ///
    /// [`FigureError::NoSuchJoint`] for a joint the rig does not have, and
    /// [`FigureError::LegNotAChain`] where the knee does not hang from the
    /// hip or the ankle from the knee — a chain no two-bone solve could run
    /// along.
    pub fn new(rigging: &Rigging<'_>, legs: [Leg; 2]) -> Result<Self, FigureError> {
        let rig = rigging.rig();
        let joints = rig.joints();
        let mut frames = Frames::new();
        crate::rig::Posture::rest(rig).resolve(Resolved::REST, &mut frames);

        let mut thigh = [0.0; 2];
        let mut shank = [0.0; 2];
        let mut hip = [Body::ORIGIN; 2];
        for (index, leg) in legs.iter().enumerate() {
            let knee = joints
                .get(leg.knee.index())
                .ok_or(FigureError::NoSuchJoint)?;
            let ankle = joints
                .get(leg.ankle.index())
                .ok_or(FigureError::NoSuchJoint)?;
            if joints.get(leg.hip.index()).is_none() {
                return Err(FigureError::NoSuchJoint);
            }
            if knee.parent != Some(leg.hip) || ankle.parent != Some(leg.knee) {
                return Err(FigureError::LegNotAChain);
            }
            thigh[index] = knee.at.length();
            shank[index] = ankle.at.length();
            hip[index] = frames.get(leg.hip).ok_or(FigureError::NoSuchJoint)?.at;
        }

        // A leg spans from straight to fully folded; the difference is how
        // much height a foot can pick up or give away without the root
        // moving, and so is the reach the tilt is a fallback for.
        let mut reach = f64::MAX;
        for index in 0..2 {
            let (thigh, shank) = (thigh[index], shank[index]);
            let folded = rigging
                .angle_for(legs[index].bend, 1.0)
                .ok_or(FigureError::NoSuchJoint)?;
            let shortest = span(thigh, shank, folded);
            reach = mathf::fmin(reach, thigh + shank - shortest);
        }
        // A straight leg from the hip is what puts the sole on the ground, so
        // the height it rests at is the rig's own statement of where its feet
        // are, not a constant beside it.
        let straight = thigh[0] + shank[0];
        let sole = hip[0].up - straight;

        Ok(Self {
            sides: legs,
            thigh,
            shank,
            hip,
            sole,
            reach,
            straight,
        })
    }

    /// The greatest height difference between two feet the legs can absorb
    /// without the figure leaning.
    #[must_use]
    pub const fn reach(&self) -> f64 {
        self.reach
    }

    /// How far apart the feet stand across the figure.
    #[must_use]
    pub fn stance(&self) -> f64 {
        mathf::fabs(self.hip[0].side - self.hip[1].side)
    }

    /// The height an ankle sits at with its leg straight and its sole on the
    /// ground.
    ///
    /// The foot's own thickness, as the rig states it: what a clip's root
    /// height is measured against.
    #[must_use]
    pub const fn sole(&self) -> f64 {
        self.sole
    }

    /// How long a leg is, straight, from hip to ankle.
    ///
    /// What a clip's root height is a fraction of, so the same curve holds
    /// on a taller figure without being rescaled by hand.
    #[must_use]
    pub const fn straight(&self) -> f64 {
        self.straight
    }

    /// Where each ankle sits in the figure's frame, for the resolve in
    /// `frames`.
    ///
    /// What a caller samples the terrain under: the foot's plan position is
    /// the animation's, so the ground that matters is the ground beneath
    /// where the clip actually put the foot.
    ///
    /// # Errors
    ///
    /// [`FigureError::NoSuchJoint`] if the resolve did not cover an ankle.
    pub fn standing(&self, frames: &Frames) -> Result<[Body; 2], FigureError> {
        let mut at = [Body::ORIGIN; 2];
        for (slot, leg) in at.iter_mut().zip(self.sides) {
            *slot = frames.get(leg.ankle).ok_or(FigureError::NoSuchJoint)?.at;
        }
        Ok(at)
    }

    /// Solve both legs so each foot meets its own ground.
    ///
    /// `pose` is the articulation every other layer has already finished
    /// with, `frames` its resolve at rest, `ground` the terrain height under
    /// each foot relative to the figure's own ground point — the heights a
    /// caller sampled at [`Self::standing`] — and `lift` the height the clip
    /// holds the body at, as a fraction of [`Self::straight`]
    /// ([`Clip::root_at`][root]).
    ///
    /// Each foot is asked for the height the clip put it at, raised by its
    /// own terrain, so a planted foot lands and a swing foot keeps its arc
    /// without either having to be told apart from the other.
    ///
    /// # Errors
    ///
    /// [`FigureError::GroundUnreal`] for a height that is not finite,
    /// [`FigureError::LiftOutsideRange`] for a root beyond a straight leg
    /// either way, [`FigureError::NoSuchJoint`] if the resolve did not
    /// cover a leg, and [`FigureError::GeometryUnreal`] for a resolve that is
    /// not finite.
    ///
    /// [root]: crate::clip::Clip::root_at
    pub fn plant(
        &self,
        rigging: &Rigging<'_>,
        pose: &Pose,
        frames: &Frames,
        ground: [f64; 2],
        lift: f64,
    ) -> Result<Planted, FigureError> {
        if !ground[0].is_finite() || !ground[1].is_finite() {
            return Err(FigureError::GroundUnreal);
        }
        if !lift.is_finite() || !(-1.0..=1.0).contains(&lift) {
            return Err(FigureError::LiftOutsideRange);
        }
        let standing = self.standing(frames)?;

        let height = lift * self.straight;
        // The lowest ground sets how far the hips come down, because the leg
        // reaching it is the one that cannot stretch; the authored height is
        // where the body sits above that. Level ground leaves the figure at
        // the height the clip asked for and nowhere else.
        let drop = height + mathf::fmin(0.0, mathf::fmin(ground[0], ground[1]));
        // What the legs cannot absorb, the figure leans into. Positive roll
        // raises the left foot, which is the one to raise when it is higher.
        let difference = ground[0] - ground[1];
        let excess = mathf::fabs(difference) - self.reach;
        let lean = if excess > 0.0 {
            let sign = if difference > 0.0 { 1.0 } else { -1.0 };
            sign * mathf::asin(mathf::clamp(excess / self.stance(), -1.0, 1.0))
        } else {
            0.0
        };
        let root = Resolved::rooted(Body::new(0.0, 0.0, drop), Rotation::new(0.0, 0.0, lean))?;

        let mut out = *pose;
        let mut miss = [0.0; 2];
        for index in 0..2 {
            let leg = self.sides[index];
            // The root sits above the whole chain, so a hip under it is
            // just where the resolve already put it, carried through — no
            // second resolve needed.
            let carried = frames.get(leg.hip).ok_or(FigureError::NoSuchJoint)?.at;
            let hip = root.at.plus(root.basis.apply(carried));
            let parent = Self::parent_basis(rigging, leg, frames, root)?;

            // Where the clip put this foot, raised by the terrain under it:
            // a planted foot lands on its own hill and a swing foot clears
            // whatever it is about to come down on.
            let target = Body::new(
                standing[index].forward,
                standing[index].side,
                height + standing[index].up + ground[index],
            );
            let solved = self.aim(
                rigging,
                index,
                parent.unapply(target.plus(hip.scaled(-1.0))),
            )?;

            write(rigging, &mut out, leg.swing, solved.pitch)?;
            write(rigging, &mut out, leg.splay, solved.roll)?;
            write(rigging, &mut out, leg.bend, solved.fold)?;
            // The foot keeps the angle the animation gave it, so a toe-off
            // stays a toe-off: the ankle turns back by however far the leg
            // above it turned.
            let turned =
                (solved.pitch + solved.fold) - Self::animated_leg_pitch(rigging, leg, pose);
            let flex = rigging
                .angle_for(leg.flex, pose.get(leg.flex))
                .ok_or(FigureError::NoSuchJoint)?;
            write(rigging, &mut out, leg.flex, flex - turned)?;

            let reached = hip.plus(parent.apply(chain(
                self.thigh[index],
                self.shank[index],
                effective(rigging, &out, leg),
            )));
            miss[index] = reached.up - target.up;
        }

        Ok(Planted {
            root,
            pose: out,
            miss,
        })
    }

    /// The frame the hip's rotations are stated in, with the root applied.
    fn parent_basis(
        rigging: &Rigging<'_>,
        leg: Leg,
        frames: &Frames,
        root: Resolved,
    ) -> Result<Basis, FigureError> {
        let parent = rigging.rig().joints()[leg.hip.index()].parent;
        let carried = match parent {
            Some(joint) => frames.get(joint).ok_or(FigureError::NoSuchJoint)?.basis,
            None => Basis::IDENTITY,
        };
        Ok(root.basis.compose(carried))
    }

    /// The hip and knee angles that put the ankle along `toward`, which is
    /// stated in the hip's parent frame.
    fn aim(
        &self,
        rigging: &Rigging<'_>,
        index: usize,
        toward: Body,
    ) -> Result<Solved, FigureError> {
        let (thigh, shank) = (self.thigh[index], self.shank[index]);
        let folded = rigging
            .angle_for(self.sides[index].bend, 1.0)
            .ok_or(FigureError::NoSuchJoint)?;
        let longest = thigh + shank;
        let shortest = span(thigh, shank, folded);
        let asked = toward.length();
        let distance = mathf::clamp(asked, shortest, longest);
        if distance <= 0.0 || asked <= 0.0 {
            return Ok(Solved::REST);
        }
        let aim = toward.scaled(1.0 / asked);

        // The triangle the hip, knee and ankle make: its knee angle folds the
        // leg to the right length, and its hip angle is how far the thigh
        // sits off the line to the foot.
        let fold = mathf::acos(
            (distance * distance - thigh * thigh - shank * shank) / (2.0 * thigh * shank),
        );
        let off = mathf::acos(
            (distance * distance + thigh * thigh - shank * shank) / (2.0 * distance * thigh),
        );

        // A leg hangs down its own frame and folds backward, so the chain's
        // end lies `off` behind the thigh. Inverting that composition for the
        // pitch and roll that land it on `aim` is one arcsine and one
        // arctangent rather than a matrix solve.
        let (behind, along) = (mathf::sin(off), mathf::cos(off));
        let roll = mathf::asin(mathf::clamp(
            if along == 0.0 { 0.0 } else { aim.side / along },
            -1.0,
            1.0,
        ));
        let across = along * mathf::cos(roll);
        let pitch = mathf::atan2(
            behind * aim.up - across * aim.forward,
            -(behind * aim.forward + across * aim.up),
        );
        Ok(Solved { pitch, roll, fold })
    }

    /// How far the leg above the ankle was already turned by the animation.
    fn animated_leg_pitch(rigging: &Rigging<'_>, leg: Leg, pose: &Pose) -> f64 {
        let swing = rigging
            .angle_for(leg.swing, pose.get(leg.swing))
            .unwrap_or(0.0);
        let bend = rigging
            .angle_for(leg.bend, pose.get(leg.bend))
            .unwrap_or(0.0);
        swing + bend
    }
}

/// A leg's solved angles.
#[derive(Copy, Clone, Debug, PartialEq)]
struct Solved {
    pitch: f64,
    roll: f64,
    fold: f64,
}

impl Solved {
    const REST: Self = Self {
        pitch: 0.0,
        roll: 0.0,
        fold: 0.0,
    };
}

/// How far apart a two-bone chain's ends are when it is folded by `fold`.
fn span(thigh: f64, shank: f64, fold: f64) -> f64 {
    mathf::sqrt(thigh * thigh + shank * shank + 2.0 * thigh * shank * mathf::cos(fold))
}

/// Where a two-bone chain folded by `fold` reaches, in the hip's own frame.
fn chain(thigh: f64, shank: f64, solved: Solved) -> Body {
    let end = Body::new(
        -shank * mathf::sin(solved.fold),
        0.0,
        -(thigh + shank * mathf::cos(solved.fold)),
    );
    Basis::of(Rotation::new(solved.pitch, 0.0, solved.roll)).apply(end)
}

/// The angles the pose actually ended up holding, which is what the limits
/// left of what the solve asked for.
///
/// An axis this rig does not drive holds *rest*, never what was asked for:
/// reading back the request there would report a foot as landed when the
/// joint that was meant to put it there never turned.
fn effective(rigging: &Rigging<'_>, pose: &Pose, leg: Leg) -> Solved {
    let held = |param: Param| rigging.angle_for(param, pose.get(param)).unwrap_or(0.0);
    Solved {
        pitch: held(leg.swing),
        roll: held(leg.splay),
        fold: held(leg.bend),
    }
}

/// Set `param` to whatever value turns it through `angle`, as far as it goes.
///
/// A target past the joint's travel writes the end of it, which is what makes
/// the miss a reported number rather than a refused frame.
fn write(
    rigging: &Rigging<'_>,
    pose: &mut Pose,
    param: Param,
    angle: f64,
) -> Result<(), FigureError> {
    let Some(value) = rigging.value_for(param, angle) else {
        return Ok(());
    };
    pose.set(param, param.range().clamp(value))
}

#[cfg(test)]
#[path = "plant/tests.rs"]
mod tests;
