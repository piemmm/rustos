//! Pose parameters: the named scalars an animation is authored in, and the
//! subsets a blend writes through.
//!
//! A clip keys *parameters*, never joint rotations. A parameter is a fraction
//! of a joint's own documented travel, so "the elbow is fully bent" is one
//! number that means the same thing on every rig that has an elbow, and the
//! rotation it becomes is whatever that rig's limit allows. The rotation a
//! clip would otherwise have to name is the rig's business, which is why an
//! elbow cannot be bent backwards here: there is no value that spells it.

use tairix_util::mathf;

use crate::error::FigureError;
use crate::socket::Side;

/// One named scalar of a pose.
///
/// Rotational only. Where the figure's root sits — a jump's lift, the pelvis
/// drop of a crouch, a clip's own displacement — is not a parameter, because
/// it cannot be decided without the ground the feet are planted on; it
/// belongs with the terrain solve rather than split across both.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Param {
    /// Fore-and-aft bend of the spine.
    SpineBend,
    /// Twist of the spine about the vertical.
    SpineTwist,
    /// Sideways tilt of the spine.
    SpineTilt,
    /// Turn of the head, left and right.
    HeadTurn,
    /// Nod of the head, up and down.
    HeadNod,
    /// Tilt of the head toward a shoulder.
    HeadTilt,
    /// Fore-and-aft swing of an arm.
    ShoulderSwing(Side),
    /// An arm lifted away from the body, or drawn across it.
    ShoulderSplay(Side),
    /// How far an elbow is folded.
    ElbowBend(Side),
    /// Bend of a wrist.
    WristAngle(Side),
    /// Fore-and-aft swing of a leg.
    HipSwing(Side),
    /// A leg lifted away from the body, or drawn across it.
    HipSplay(Side),
    /// How far a knee is folded.
    KneeBend(Side),
    /// Flex of an ankle.
    AnkleAngle(Side),
    /// A tail raised from where it hangs, or let droop.
    TailLift,
    /// A tail swung to one side.
    TailSwing,
}

impl Param {
    /// How many parameters a pose holds.
    pub const COUNT: usize = 24;

    /// Every parameter, in the order a pose holds them.
    pub const ALL: [Self; Self::COUNT] = [
        Self::SpineBend,
        Self::SpineTwist,
        Self::SpineTilt,
        Self::HeadTurn,
        Self::HeadNod,
        Self::HeadTilt,
        Self::ShoulderSwing(Side::Left),
        Self::ShoulderSwing(Side::Right),
        Self::ShoulderSplay(Side::Left),
        Self::ShoulderSplay(Side::Right),
        Self::ElbowBend(Side::Left),
        Self::ElbowBend(Side::Right),
        Self::WristAngle(Side::Left),
        Self::WristAngle(Side::Right),
        Self::HipSwing(Side::Left),
        Self::HipSwing(Side::Right),
        Self::HipSplay(Side::Left),
        Self::HipSplay(Side::Right),
        Self::KneeBend(Side::Left),
        Self::KneeBend(Side::Right),
        Self::AnkleAngle(Side::Left),
        Self::AnkleAngle(Side::Right),
        Self::TailLift,
        Self::TailSwing,
    ];

    /// Its position in a pose's own table.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::SpineBend => 0,
            Self::SpineTwist => 1,
            Self::SpineTilt => 2,
            Self::HeadTurn => 3,
            Self::HeadNod => 4,
            Self::HeadTilt => 5,
            Self::ShoulderSwing(side) => 6 + side as usize,
            Self::ShoulderSplay(side) => 8 + side as usize,
            Self::ElbowBend(side) => 10 + side as usize,
            Self::WristAngle(side) => 12 + side as usize,
            Self::HipSwing(side) => 14 + side as usize,
            Self::HipSplay(side) => 16 + side as usize,
            Self::KneeBend(side) => 18 + side as usize,
            Self::AnkleAngle(side) => 20 + side as usize,
            Self::TailLift => 22,
            Self::TailSwing => 23,
        }
    }

    /// The interval its values are authored in.
    #[must_use]
    pub const fn range(self) -> Range {
        match self {
            Self::ElbowBend(_) | Self::KneeBend(_) => Range::Unit,
            _ => Range::Signed,
        }
    }
}

/// The interval a parameter's values are authored in.
///
/// The two cases are the two kinds of joint: one that folds a single way, and
/// one that travels either side of rest.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Range {
    /// `0..=1`, for a joint that folds one way only.
    Unit,
    /// `-1..=1`, for a joint that travels either side of rest.
    Signed,
}

impl Range {
    /// Its lower bound.
    #[must_use]
    pub const fn min(self) -> f64 {
        match self {
            Self::Unit => 0.0,
            Self::Signed => -1.0,
        }
    }

    /// Its upper bound.
    #[must_use]
    pub const fn max(self) -> f64 {
        1.0
    }

    /// Whether `value` is within it.
    ///
    /// A value that is not a finite number is outside every range, which is
    /// what keeps it out of the rotation it would become.
    #[must_use]
    pub fn holds(self, value: f64) -> bool {
        value.is_finite() && value >= self.min() && value <= self.max()
    }

    /// `value` brought inside the range.
    ///
    /// For interpolation and blending only, where the result is a weighted
    /// mean of values already inside and so is mathematically inside too:
    /// this absorbs the last bit of rounding rather than failing a pose that
    /// is correct. Authored values go through [`Pose::set`], which refuses
    /// an out-of-range value instead of quietly moving it.
    #[must_use]
    pub fn clamp(self, value: f64) -> f64 {
        mathf::clamp(value, self.min(), self.max())
    }
}

/// Every parameter at once: one figure's articulation, as numbers.
///
/// Rest is all zeroes, so a default pose is the rig's own neutral and a
/// parameter nothing writes simply stays there.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Pose {
    values: [f64; Param::COUNT],
}

impl Pose {
    /// Every parameter at rest.
    pub const REST: Self = Self {
        values: [0.0; Param::COUNT],
    };

    /// What `param` is set to.
    #[must_use]
    pub fn get(&self, param: Param) -> f64 {
        self.values[param.index()]
    }

    /// Set `param` to `value`.
    ///
    /// # Errors
    ///
    /// [`FigureError::ParamOutsideRange`] for a value outside what the
    /// parameter is authored in, which is where a clip that would drive a
    /// joint past its travel fails.
    pub fn set(&mut self, param: Param, value: f64) -> Result<(), FigureError> {
        if !param.range().holds(value) {
            return Err(FigureError::ParamOutsideRange);
        }
        self.values[param.index()] = value;
        Ok(())
    }

    /// The same pose with `param` set to `value`.
    ///
    /// # Errors
    ///
    /// As [`Self::set`].
    pub fn with(mut self, param: Param, value: f64) -> Result<Self, FigureError> {
        self.set(param, value)?;
        Ok(self)
    }
}

impl Default for Pose {
    fn default() -> Self {
        Self::REST
    }
}

/// Which parameters something writes.
///
/// What makes a cast play on the upper body while the legs keep walking: the
/// blend weighs each parameter only against the clips that claimed it, so an
/// unmasked parameter is not dragged toward rest by a clip that never had an
/// opinion about it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Mask(u32);

const _: () = assert!(Param::COUNT <= u32::BITS as usize);

impl Mask {
    /// No parameters.
    pub const NONE: Self = Self(0);

    /// Every parameter.
    pub const ALL: Self = Self(u32::MAX >> (u32::BITS as usize - Param::COUNT));

    /// The same mask, with `param` added.
    #[must_use]
    pub const fn with(self, param: Param) -> Self {
        Self(self.0 | (1 << param.index()))
    }

    /// The same mask, with `param` removed.
    #[must_use]
    pub const fn without(self, param: Param) -> Self {
        Self(self.0 & !(1 << param.index()))
    }

    /// Whether it holds `param`.
    #[must_use]
    pub const fn holds(self, param: Param) -> bool {
        self.0 & (1 << param.index()) != 0
    }

    /// Everything in either.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Everything in both.
    #[must_use]
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Everything in this one that is not in `other`.
    #[must_use]
    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Whether it holds nothing.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// How many parameters it holds.
    #[must_use]
    pub const fn len(self) -> usize {
        self.0.count_ones() as usize
    }
}

/// Procedural deltas gathered over an articulated pose.
///
/// The layers above a clip — breathing, look-at, recoil — each state their
/// effect as a signed delta per parameter rather than as a value, so they sum
/// instead of overwriting one another and their order does not change the
/// answer. The sum lands back inside the parameter's own range, which is what
/// carries the in-limit guarantee through any number of layers: a delta is a
/// further fraction of the joint's travel, and a fraction clamped to one is
/// still the joint's own limit rather than past it.
///
/// A delta is not an angle. Parameter travel is scaled per side of rest, so a
/// delta means "this much further toward that end of what the joint can do" —
/// which is why a layer crowded out by a pose already at its extreme fades
/// instead of fighting it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Overlay {
    delta: [f64; Param::COUNT],
}

impl Overlay {
    /// No layer has written anything.
    pub const NONE: Self = Self {
        delta: [0.0; Param::COUNT],
    };

    /// Add `delta` to what `param` has gathered.
    ///
    /// # Errors
    ///
    /// [`FigureError::ParamOutsideRange`] for a delta that is not finite.
    /// A delta is unbounded otherwise: it is the sum that must land in range,
    /// not each contribution, and two layers pulling opposite ways are
    /// entitled to cancel.
    pub fn add(&mut self, param: Param, delta: f64) -> Result<(), FigureError> {
        if !delta.is_finite() {
            return Err(FigureError::ParamOutsideRange);
        }
        self.delta[param.index()] += delta;
        Ok(())
    }

    /// What `param` has gathered.
    #[must_use]
    pub fn get(&self, param: Param) -> f64 {
        self.delta[param.index()]
    }

    /// The parameters some layer has moved.
    #[must_use]
    pub fn written(&self) -> Mask {
        let mut mask = Mask::NONE;
        for param in Param::ALL {
            if self.delta[param.index()] != 0.0 {
                mask = mask.with(param);
            }
        }
        mask
    }

    /// Whether no layer has moved anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.written().is_empty()
    }

    /// Everything `other` gathered, added to this.
    ///
    /// Summing overlays rather than applying them one after another is what
    /// keeps a layer's own effect independent of where it sits in the stack.
    pub fn absorb(&mut self, other: &Self) {
        for param in Param::ALL {
            self.delta[param.index()] += other.delta[param.index()];
        }
    }

    /// `pose` with every gathered delta added, each brought into range.
    ///
    /// # Errors
    ///
    /// Cannot fail for deltas this accumulator accepted: a finite sum brought
    /// into a parameter's range is in it. The result is a `Result` because
    /// [`Pose::set`] is the one gate on a value and the crate keeps no second
    /// way past it.
    pub fn applied(&self, pose: &Pose) -> Result<Pose, FigureError> {
        let mut out = *pose;
        for param in Param::ALL {
            let delta = self.delta[param.index()];
            if delta != 0.0 {
                out.set(param, param.range().clamp(pose.get(param) + delta))?;
            }
        }
        Ok(out)
    }
}

impl Default for Overlay {
    fn default() -> Self {
        Self::NONE
    }
}

#[cfg(test)]
#[path = "pose/tests.rs"]
mod tests;
