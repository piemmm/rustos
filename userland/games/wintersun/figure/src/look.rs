//! Turning the head toward something.
//!
//! A figure whose head stays square while a fight happens beside it reads as
//! scenery. Aiming the head at what the character is attending to is the
//! cheapest thing that makes it read as attending to it.
//!
//! The aim is stated as a *delta* from where the head already points, so it
//! composes with whatever clip is playing rather than fighting it, and the
//! spine takes a share of the turn — which is what lets a figure look behind
//! itself at all, since a neck alone cannot.

use tairix_util::mathf;

use crate::error::FigureError;
use crate::frame::Body;
use crate::joint::JointId;
use crate::pose::{Overlay, Param};
use crate::rig::Frames;
use crate::rigging::Rigging;

/// Nearer than this to the head and a target has no direction worth aiming
/// at, so the aim is left alone rather than snapping to a rounding artefact.
const NEAREST: f64 = 1e-6;

/// How a figure turns to look at something.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Look {
    share: f64,
}

impl Look {
    /// A look whose spine takes `share` of the turn and whose head takes the
    /// rest.
    ///
    /// The two shares always sum to the whole turn, so the head still ends up
    /// aimed wherever the target is however the work was divided — the share
    /// decides how the figure looks doing it, not where it ends up looking.
    ///
    /// # Errors
    ///
    /// [`FigureError::ParamOutsideRange`] for a share outside `0..=1`.
    pub fn new(share: f64) -> Result<Self, FigureError> {
        if !share.is_finite() || !(0.0..=1.0).contains(&share) {
            return Err(FigureError::ParamOutsideRange);
        }
        Ok(Self { share })
    }

    /// How much of the turn the spine takes.
    #[must_use]
    pub const fn share(self) -> f64 {
        self.share
    }

    /// Aim `head` at `target`, stated in the figure's own frame.
    ///
    /// The result is a delta over whatever the clip is already doing, and it
    /// is deliberately not clamped here: a target behind a figure's shoulder
    /// asks for more turn than a neck has, and it is
    /// [`Overlay::applied`] bringing the sum into range that stops the head
    /// at the limit. A figure therefore turns *as far as it can* toward
    /// something out of reach rather than giving up on it.
    ///
    /// # Errors
    ///
    /// [`FigureError::NoSuchJoint`] if the rig has no such joint or the
    /// resolve did not cover it, and [`FigureError::GeometryUnreal`] for a
    /// target that is not finite.
    pub fn at(
        self,
        rigging: &Rigging<'_>,
        frames: &Frames,
        head: JointId,
        target: Body,
    ) -> Result<Overlay, FigureError> {
        if !target.is_real() {
            return Err(FigureError::GeometryUnreal);
        }
        let frame = frames.get(head).ok_or(FigureError::NoSuchJoint)?;
        let toward = frame.basis.unapply(target.plus(frame.at.scaled(-1.0)));
        let distance = toward.length();
        if distance <= NEAREST {
            return Ok(Overlay::NONE);
        }
        let aim = toward.scaled(1.0 / distance);

        // The head's own forward axis is where it already looks, so these are
        // the angles still to turn through. Yaw first, then pitch about the
        // axis the yaw left, which is the order a rotation composes in.
        let yaw = mathf::atan2(aim.side, aim.forward);
        let nod = -mathf::asin(aim.up);

        let mut overlay = Overlay::NONE;
        write(rigging, &mut overlay, Param::SpineTwist, yaw * self.share)?;
        write(
            rigging,
            &mut overlay,
            Param::HeadTurn,
            yaw * (1.0 - self.share),
        )?;
        write(rigging, &mut overlay, Param::HeadNod, nod)?;
        Ok(overlay)
    }
}

/// Add the value that turns `param` through `angle`, if this rig turns it.
///
/// A rig that does not drive the parameter simply does not take that share of
/// the turn, which is how a figure with no waist still looks up.
fn write(
    rigging: &Rigging<'_>,
    overlay: &mut Overlay,
    param: Param,
    angle: f64,
) -> Result<(), FigureError> {
    match rigging.value_for(param, angle) {
        Some(value) => overlay.add(param, value),
        None => Ok(()),
    }
}

#[cfg(test)]
#[path = "look/tests.rs"]
mod tests;
