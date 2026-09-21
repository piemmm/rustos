//! What this crate refuses, and why.

use core::fmt;

/// A refusal from the figure engine.
///
/// Every variant is a rig or a request that could only draw something nobody
/// authored, so each is refused at construction rather than drawn.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FigureError {
    /// More joints than a rig holds.
    TooManyJoints,
    /// More parts than a rig holds.
    TooManyParts,
    /// A second mount for a socket that already has one.
    DuplicateSocket,
    /// More fitted equipment than one figure carries.
    TooMuchEquipment,
    /// A joint naming a parent the rig does not have.
    NoSuchParent,
    /// A joint naming a parent that does not precede it.
    ///
    /// Parents first makes the hierarchy a forest — no cycle can be spelled —
    /// and lets a posture resolve in one forward pass.
    ParentNotEarlier,
    /// A rotation limit whose lower bound exceeds its upper one.
    LimitInverted,
    /// A rotation limit reaching beyond a full turn, or not a finite number.
    LimitUnreal,
    /// A rotation limit that excludes zero, so the joint cannot rest.
    LimitExcludesRest,
    /// A rotation outside the limit its joint documents.
    RotationOutsideLimit,
    /// A part, socket or posture entry naming a joint the rig does not have.
    NoSuchJoint,
    /// A joint that bears a child but carries no part of its own.
    ///
    /// The mass a limb grows out of: without it a swinging shank opens a gap
    /// where it meets the body.
    BearingJointWithoutMass,
    /// A child joint whose origin lies beyond everything its parent draws,
    /// which is a gap in the silhouette however the figure is posed.
    JointBeyondParentReach,
    /// A geometry value that is not a finite number.
    ///
    /// Refused because it would otherwise reach the depth sort and the
    /// rasteriser as a position no pixel corresponds to.
    GeometryUnreal,
    /// Equipment naming a socket this rig does not offer.
    NoSuchSocket,
    /// A scale that is not a finite positive number.
    ScaleUnreal,
}

impl fmt::Display for FigureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::TooManyJoints => "more joints than a rig holds",
            Self::TooManyParts => "more parts than a rig holds",
            Self::DuplicateSocket => "socket already mounted",
            Self::TooMuchEquipment => "more equipment than a figure carries",
            Self::NoSuchParent => "no such parent joint",
            Self::ParentNotEarlier => "parent joint does not precede its child",
            Self::LimitInverted => "rotation limit inverted",
            Self::LimitUnreal => "rotation limit beyond a full turn or not finite",
            Self::LimitExcludesRest => "rotation limit excludes rest",
            Self::RotationOutsideLimit => "rotation outside its joint's limit",
            Self::NoSuchJoint => "no such joint",
            Self::BearingJointWithoutMass => "joint bears a child but carries no mass",
            Self::JointBeyondParentReach => "child joint beyond its parent's reach",
            Self::GeometryUnreal => "geometry value not finite",
            Self::NoSuchSocket => "rig offers no such socket",
            Self::ScaleUnreal => "scale not a finite positive number",
        };
        f.write_str(text)
    }
}
