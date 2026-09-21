//! Binding pose parameters to a rig's joints.
//!
//! A drive says which joint axis a parameter turns and which way its `+1`
//! points, and the angle it produces is that fraction of the joint's *own*
//! documented travel. Two consequences carry the design: one clip drives any
//! rig that declares the same parameters, and no value of any parameter can
//! leave a limit — so a posture built here has no out-of-limit case, and the
//! handedness of an outward splay is stated once here rather than in every
//! clip that lifts an arm.

use crate::error::FigureError;
use crate::frame::Rotation;
use crate::joint::{JointId, Limit, Limits, MAX_JOINTS};
use crate::pose::{Mask, Param, Pose};
use crate::rig::{Posture, Rig};

/// Which axis of a joint a drive turns.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Axis {
    /// Fore-and-aft swing.
    Pitch,
    /// Turn about the vertical.
    Yaw,
    /// Splay about the joint's own forward axis.
    Roll,
}

impl Axis {
    /// Every axis.
    pub const ALL: [Self; 3] = [Self::Pitch, Self::Yaw, Self::Roll];

    /// Its position among the three.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Pitch => 0,
            Self::Yaw => 1,
            Self::Roll => 2,
        }
    }

    /// That axis of `limits`, as an interval.
    #[must_use]
    const fn of(self, limits: Limits) -> Limit {
        match self {
            Self::Pitch => limits.pitch,
            Self::Yaw => limits.yaw,
            Self::Roll => limits.roll,
        }
    }

    /// Write `angle` into that axis of `rotation`.
    fn write(self, rotation: &mut Rotation, angle: f64) {
        match self {
            Self::Pitch => rotation.pitch = angle,
            Self::Yaw => rotation.yaw = angle,
            Self::Roll => rotation.roll = angle,
        }
    }
}

/// Which end of a joint's travel a parameter's `+1` reaches.
///
/// An enum rather than a signed multiplier because there are exactly two
/// answers and a third would have no meaning.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Sense {
    /// `+1` reaches the limit's upper bound.
    Forward,
    /// `+1` reaches its lower bound, for a joint whose travel is the other
    /// way — an elbow that only closes, or an outward splay on the side
    /// where outward is negative.
    Reverse,
}

/// One parameter's effect on one joint axis.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Drive {
    /// The parameter that turns it.
    pub param: Param,
    /// The joint it turns.
    pub joint: JointId,
    /// Which of that joint's axes.
    pub axis: Axis,
    /// Which end of the axis's travel the parameter's `+1` reaches.
    pub sense: Sense,
}

impl Drive {
    /// A drive of `axis` on `joint` by `param`, `+1` reaching the upper
    /// bound.
    #[must_use]
    pub const fn new(param: Param, joint: JointId, axis: Axis) -> Self {
        Self {
            param,
            joint,
            axis,
            sense: Sense::Forward,
        }
    }

    /// The same drive with `+1` reaching the lower bound instead.
    #[must_use]
    pub const fn reversed(mut self) -> Self {
        self.sense = Sense::Reverse;
        self
    }
}

/// A rig and the table binding pose parameters to its joints.
///
/// Borrows both, so a pose can only be resolved against the rig whose limits
/// scale it.
#[derive(Copy, Clone, Debug)]
pub struct Rigging<'a> {
    rig: &'a Rig,
    drives: &'a [Drive],
    driven: Mask,
}

impl<'a> Rigging<'a> {
    /// Bind `drives` to `rig`.
    ///
    /// # Errors
    ///
    /// [`FigureError::NoSuchJoint`] for a drive naming a joint the rig does
    /// not have, and [`FigureError::DriveCollision`] for two drives on one
    /// joint axis — which would let two parameters sum past a limit, and is
    /// the one thing that could put a posture out of range.
    pub fn new(rig: &'a Rig, drives: &'a [Drive]) -> Result<Self, FigureError> {
        let joints = rig.joints().len();
        let mut claimed = [0u8; MAX_JOINTS];
        let mut declared = Mask::NONE;

        for drive in drives {
            let index = drive.joint.index();
            if index >= joints {
                return Err(FigureError::NoSuchJoint);
            }
            let bit = 1u8 << drive.axis.index();
            if claimed[index] & bit != 0 {
                return Err(FigureError::DriveCollision);
            }
            claimed[index] |= bit;
            declared = declared.with(drive.param);
        }

        Ok(Self {
            rig,
            drives,
            driven: declared,
        })
    }

    /// The rig it binds to.
    #[must_use]
    pub const fn rig(&self) -> &'a Rig {
        self.rig
    }

    /// The parameters it actually turns something with.
    ///
    /// A parameter outside this drives nothing on this rig, which is how a
    /// clip authored for a richer skeleton still plays on a simpler one.
    #[must_use]
    pub const fn driven(&self) -> Mask {
        self.driven
    }

    /// The total angle `param` at `value` turns through on this rig.
    ///
    /// A parameter may drive several joints — a head turn is shared between
    /// the neck and the atlas, a spine bend between the waist and the chest —
    /// so the angle that matters to a layer aiming at something is the sum
    /// over the chain, not any one joint's share.
    ///
    /// `None` if `param` drives nothing here, which is how a clip authored
    /// for a richer skeleton still plays on a simpler one.
    #[must_use]
    pub fn angle_for(&self, param: Param, value: f64) -> Option<f64> {
        let mut total = 0.0;
        let mut driven = false;
        for drive in self.drives.iter().filter(|d| d.param == param) {
            let limits = self.rig.joints()[drive.joint.index()].limits;
            total += scale(drive.axis.of(limits), value, drive.sense);
            driven = true;
        }
        driven.then_some(total)
    }

    /// The value of `param` whose total turn is `angle`.
    ///
    /// The inverse of [`Self::angle_for`], so a layer that works in angles —
    /// an inverse-kinematic solve aiming a limb at a point, a look-at turning
    /// a head toward a target — hands its answer back in the parameter domain
    /// rather than writing a rotation behind the pose's back.
    ///
    /// Deliberately *not* clamped: a target the chain cannot reach answers
    /// past `1`, which is the caller's signal that it asked for more than the
    /// body has and must report the shortfall rather than pretend.
    ///
    /// `None` if `param` drives nothing here, or if its travel does not go
    /// the way `angle` asks — an axis that only folds one way has no value
    /// that unfolds it.
    #[must_use]
    pub fn value_for(&self, param: Param, angle: f64) -> Option<f64> {
        if !angle.is_finite() {
            return None;
        }
        // Travel is scaled per side of rest, so the parameter is linear on
        // each side and these two ends are the whole mapping. Which *sign* of
        // value reaches a given angle is the drive's business, not the
        // angle's: a reversed drive swings a limb forward on a positive
        // value, so a negative angle comes from a positive value there.
        let raised = self.angle_for(param, 1.0)?;
        let lowered = self.angle_for(param, -1.0)?;
        if angle == 0.0 {
            return Some(0.0);
        }
        if raised != 0.0 && (angle > 0.0) == (raised > 0.0) {
            return Some(angle / raised);
        }
        if lowered != 0.0 && (angle > 0.0) == (lowered > 0.0) {
            return Some(-angle / lowered);
        }
        None
    }

    /// Turn the rig to `pose`.
    ///
    /// # Errors
    ///
    /// Cannot fail for a rigging this constructor accepted: a pose's values
    /// are within their parameter's range, and an angle is that fraction of
    /// the axis's own limit, so every rotation is inside it. The result is a
    /// `Result` because [`Posture::set`] is the crate's one gate on a
    /// rotation and there is deliberately no second, unchecked way past it.
    pub fn posture(&self, pose: &Pose) -> Result<Posture<'a>, FigureError> {
        let mut rotations = [Rotation::REST; MAX_JOINTS];

        for drive in self.drives {
            let limits = self.rig.joints()[drive.joint.index()].limits;
            let angle = scale(drive.axis.of(limits), pose.get(drive.param), drive.sense);
            drive.axis.write(&mut rotations[drive.joint.index()], angle);
        }

        let mut posture = Posture::rest(self.rig);
        let mut written = [false; MAX_JOINTS];
        for drive in self.drives {
            let index = drive.joint.index();
            if !written[index] {
                written[index] = true;
                posture.set(drive.joint, rotations[index])?;
            }
        }
        Ok(posture)
    }
}

/// `value` as that fraction of `limit`, toward the end `sense` names.
///
/// Each half of the interval is scaled on its own, so rest stays rest however
/// lopsided the joint's travel is. Never leaves the limit: the magnitude is
/// at most one and rounding a product of a factor below one cannot carry it
/// past the bound.
fn scale(limit: Limit, value: f64, sense: Sense) -> f64 {
    let toward = match sense {
        Sense::Forward => value,
        Sense::Reverse => -value,
    };
    if toward >= 0.0 {
        toward * limit.max()
    } else {
        -toward * limit.min()
    }
}

#[cfg(test)]
#[path = "rigging/tests.rs"]
mod tests;
