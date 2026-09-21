//! Joints: the hierarchy a figure's parts are carried by, and the limits that
//! keep a pose anatomically possible.

use core::f64::consts::TAU;

use crate::error::FigureError;
use crate::frame::{Body, Rotation};

/// How many joints one rig holds.
///
/// A bound on first-party authored content rather than a capacity: a rig is
/// code, not input, and the shipped rigs are held to this at build time. A
/// figure needing more joints than this is a different figure, not a bigger
/// one.
pub const MAX_JOINTS: usize = 32;

const _: () = assert!(MAX_JOINTS <= u8::MAX as usize);

/// Which joint, by position in its rig's own table.
///
/// Every `u8` names a position, so an id has no invalid value and no
/// constructor to fail. Whether a rig *has* that joint is the rig's question,
/// answered once when it is assembled — after which every part, mount and
/// posture entry in it is known to be in range.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct JointId(u8);

impl JointId {
    /// The joint at `index`.
    #[must_use]
    pub const fn new(index: u8) -> Self {
        Self(index)
    }

    /// Its position in the rig's table.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// How far one axis of a joint may turn from rest, in radians.
///
/// A closed interval that always contains zero, so a joint can always rest;
/// a limit excluding rest would make the rig's own neutral pose illegal.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Limit {
    min: f64,
    max: f64,
}

impl Limit {
    /// An axis that does not turn.
    pub const FIXED: Self = Self { min: 0.0, max: 0.0 };

    /// A limit from `min` to `max` radians.
    ///
    /// # Errors
    ///
    /// [`FigureError::LimitUnreal`] for a bound that is not finite or reaches
    /// beyond a full turn, [`FigureError::LimitInverted`] for a lower bound
    /// above its upper one, and [`FigureError::LimitExcludesRest`] for an
    /// interval that does not contain zero.
    pub fn new(min: f64, max: f64) -> Result<Self, FigureError> {
        if !min.is_finite() || !max.is_finite() || min < -TAU || max > TAU {
            return Err(FigureError::LimitUnreal);
        }
        if min > max {
            return Err(FigureError::LimitInverted);
        }
        if min > 0.0 || max < 0.0 {
            return Err(FigureError::LimitExcludesRest);
        }
        Ok(Self { min, max })
    }

    /// A limit of `span` radians either side of rest.
    ///
    /// # Errors
    ///
    /// As [`Self::new`]; a negative `span` is inverted.
    pub fn symmetric(span: f64) -> Result<Self, FigureError> {
        Self::new(-span, span)
    }

    /// Its lower bound.
    #[must_use]
    pub const fn min(self) -> f64 {
        self.min
    }

    /// Its upper bound.
    #[must_use]
    pub const fn max(self) -> f64 {
        self.max
    }

    /// Whether `angle` is within it.
    ///
    /// A non-finite angle is outside every limit, which is what keeps it out
    /// of the projection.
    #[must_use]
    pub fn holds(self, angle: f64) -> bool {
        angle.is_finite() && angle >= self.min && angle <= self.max
    }
}

/// How far a joint may turn from rest, on each of its three axes.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Limits {
    /// Fore-and-aft swing.
    pub pitch: Limit,
    /// Turn about the vertical.
    pub yaw: Limit,
    /// Splay about the joint's own forward axis.
    pub roll: Limit,
}

impl Limits {
    /// A joint that does not turn at all: a rigidly carried part.
    pub const FIXED: Self = Self {
        pitch: Limit::FIXED,
        yaw: Limit::FIXED,
        roll: Limit::FIXED,
    };

    /// Limits from three intervals.
    #[must_use]
    pub const fn new(pitch: Limit, yaw: Limit, roll: Limit) -> Self {
        Self { pitch, yaw, roll }
    }

    /// A joint that only swings fore-and-aft, `span` radians either way.
    ///
    /// # Errors
    ///
    /// As [`Limit::symmetric`].
    pub fn hinge(span: f64) -> Result<Self, FigureError> {
        Ok(Self::new(
            Limit::symmetric(span)?,
            Limit::FIXED,
            Limit::FIXED,
        ))
    }

    /// Whether `rotation` is within all three.
    #[must_use]
    pub fn holds(self, rotation: Rotation) -> bool {
        self.pitch.holds(rotation.pitch)
            && self.yaw.holds(rotation.yaw)
            && self.roll.holds(rotation.roll)
    }
}

/// One joint of a rig: where it sits, how it rests, and how far it turns.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Joint {
    /// The joint that carries it, or `None` for the root.
    ///
    /// A parent always precedes its child in the rig's table, which is what
    /// makes the hierarchy a forest and lets a posture resolve in one pass.
    pub parent: Option<JointId>,
    /// Where it sits in its parent's frame at rest.
    pub at: Body,
    /// How its frame is turned in its parent's frame at rest.
    ///
    /// Separate from the posture's rotation so a splayed thigh or an outward
    /// collarbone is stated once, here, rather than baked into every child
    /// offset and every part's outline where the two could drift apart.
    pub orientation: Rotation,
    /// How far it may turn from that rest.
    pub limits: Limits,
}

impl Joint {
    /// A joint at `at` in `parent`'s frame, resting square, turning within
    /// `limits`.
    #[must_use]
    pub const fn new(parent: Option<JointId>, at: Body, limits: Limits) -> Self {
        Self {
            parent,
            at,
            orientation: Rotation::REST,
            limits,
        }
    }

    /// The same joint resting at `orientation` rather than square.
    #[must_use]
    pub const fn oriented(mut self, orientation: Rotation) -> Self {
        self.orientation = orientation;
        self
    }
}

#[cfg(test)]
#[path = "joint/tests.rs"]
mod tests;
