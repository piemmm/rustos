//! Cloth, hair and tails trailing what carries them.
//!
//! A cloak modelled as part of the body turns when the body turns, which is
//! exactly what cloth does not do — it hangs where it was and catches up.
//! The difference is the whole reason a cloak reads as cloth, and it costs
//! one damped spring per axis.
//!
//! The drive is the carrier's *acceleration*, not its velocity: a figure
//! moving steadily has its cloak hanging straight behind it and nothing
//! swinging, while one that starts, stops or turns throws it. Wind adds to
//! the same drive, so a stationary figure in a gale gets the same treatment
//! with no second path.

use tairix_util::mathf;

use crate::error::FigureError;
use crate::frame::{Body, Rotation};
use crate::pose::{Overlay, Param};
use crate::rigging::Rigging;
use crate::spring::{Motion, Spring};

/// Below this, in radians per second, a hem has stopped moving.
const STILL: f64 = 1e-4;

/// A hanging element that lags what carries it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Sway {
    spring: Spring,
    give: f64,
    limit: f64,
    pitch: Motion,
    roll: Motion,
}

impl Sway {
    /// A hanging element on `spring`, leaning `give` radians per unit of
    /// drive, never past `limit` radians.
    ///
    /// The limit is a bound rather than a capacity: a hem that can swing past
    /// its own mounting has come off, and no input should be able to ask for
    /// that however hard the figure turns.
    ///
    /// # Errors
    ///
    /// [`FigureError::GeometryUnreal`] for a give that is not finite, and
    /// [`FigureError::LimitUnreal`] for a limit that is not finite and
    /// positive.
    pub fn new(spring: Spring, give: f64, limit: f64) -> Result<Self, FigureError> {
        if !give.is_finite() {
            return Err(FigureError::GeometryUnreal);
        }
        if !limit.is_finite() || limit <= 0.0 {
            return Err(FigureError::LimitUnreal);
        }
        Ok(Self {
            spring,
            give,
            limit,
            pitch: Motion::REST,
            roll: Motion::REST,
        })
    }

    /// Let `seconds` pass with the carrier accelerating at `acceleration` and
    /// the air moving at `wind`, both in the carrier's own frame.
    ///
    /// # Errors
    ///
    /// [`FigureError::GeometryUnreal`] for a drive that is not finite, and
    /// [`FigureError::ElapsedUnreal`] for a step that is not finite and
    /// non-negative.
    pub fn advance(
        &mut self,
        acceleration: Body,
        wind: Body,
        seconds: f64,
    ) -> Result<(), FigureError> {
        if !acceleration.is_real() || !wind.is_real() {
            return Err(FigureError::GeometryUnreal);
        }
        // A hem left behind by an acceleration leans against it, and blown by
        // a wind leans with it.
        let drive = wind.plus(acceleration.scaled(-1.0));
        let lean = |along: f64| mathf::clamp(self.give * along, -self.limit, self.limit);
        // A part hangs below its mount, so it swings forward on a negative
        // pitch and to the figure's left on a positive roll.
        self.pitch = self
            .spring
            .step(self.pitch, -lean(drive.forward), seconds)?;
        self.roll = self.spring.step(self.roll, lean(drive.side), seconds)?;
        Ok(())
    }

    /// How far it currently leans from its socket's rest.
    ///
    /// Clamped to the limit the spring may briefly overshoot: the overshoot
    /// is the life in the cloth, and the clamp is what keeps it cloth.
    #[must_use]
    pub fn turn(&self) -> Rotation {
        Rotation::new(
            mathf::clamp(self.pitch.value, -self.limit, self.limit),
            0.0,
            mathf::clamp(self.roll.value, -self.limit, self.limit),
        )
    }

    /// The lean as deltas on the parameters that turn a hanging part's own
    /// joint, for anatomy — a tail — rather than gear on a socket.
    ///
    /// A joint turns only through the pose, so the lean sums with the clip
    /// and every other layer and lands inside the joint's limits. `pitch` and
    /// `roll` name the parameters that turn the joint's pitch and roll; one
    /// that drives nothing on `rigging`, or cannot travel the way the lean
    /// asks, adds nothing.
    ///
    /// # Errors
    ///
    /// Cannot fail for a lean this sway holds, which is finite; the result
    /// is a `Result` because [`Overlay::add`] is the one gate on a delta.
    pub fn overlay(
        &self,
        rigging: &Rigging<'_>,
        pitch: Param,
        roll: Param,
    ) -> Result<Overlay, FigureError> {
        let turn = self.turn();
        let mut overlay = Overlay::NONE;
        for (param, angle) in [(pitch, turn.pitch), (roll, turn.roll)] {
            if let Some(value) = rigging.value_for(param, angle) {
                overlay.add(param, value)?;
            }
        }
        Ok(overlay)
    }

    /// Whether it has stopped moving.
    ///
    /// Stillness rather than any particular lean: a cloak hanging steady in
    /// a steady wind is settled, and where it is hanging is the wind's
    /// business rather than this question's.
    #[must_use]
    pub fn settled(&self) -> bool {
        mathf::fabs(self.pitch.velocity) <= STILL && mathf::fabs(self.roll.velocity) <= STILL
    }
}

#[cfg(test)]
#[path = "sway/tests.rs"]
mod tests;
