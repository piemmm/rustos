//! The kick of an impact, and the settle after it.
//!
//! An attack whose animation simply plays through feels weightless: nothing
//! in the figure registers that it hit something. A recoil is what registers
//! it — the struck limb is thrown off its keyframed path and springs back,
//! and the overshoot on the way is the follow-through.
//!
//! It is an *impulse* rather than a displacement because that is what an
//! impact is. The hand has not moved yet on the frame the blow lands; what
//! changes is how fast it is going, and the settle after is the body
//! absorbing that.

use crate::error::FigureError;
use crate::pose::{Overlay, Param};
use crate::spring::{Motion, Spring};

/// Below this, in parameter travel and travel per second, a recoil is over.
const SETTLED: f64 = 1e-4;

/// Impulses on a figure's parameters, springing back to the clip.
///
/// The target is always the animation itself, never a pose of its own: a
/// recoil says how far the body was knocked off what it was doing, so it
/// composes with any clip and disappears completely once it is spent.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Recoil {
    spring: Spring,
    motion: [Motion; Param::COUNT],
}

impl Recoil {
    /// A recoil that settles on `spring`.
    ///
    /// Give it a damping ratio below one: the overshoot is the
    /// follow-through, and a critically damped recoil is a shrug.
    #[must_use]
    pub const fn new(spring: Spring) -> Self {
        Self {
            spring,
            motion: [Motion::REST; Param::COUNT],
        }
    }

    /// The spring it settles on.
    #[must_use]
    pub const fn spring(&self) -> Spring {
        self.spring
    }

    /// Knock `param` off its path at `impulse` of its travel per second.
    ///
    /// # Errors
    ///
    /// [`FigureError::MotionUnreal`] for an impulse that is not finite.
    pub fn strike(&mut self, param: Param, impulse: f64) -> Result<(), FigureError> {
        self.motion[param.index()] = self.motion[param.index()].struck(impulse)?;
        Ok(())
    }

    /// Let `seconds` of settling pass.
    ///
    /// # Errors
    ///
    /// [`FigureError::ElapsedUnreal`] for a step that is not finite and
    /// non-negative.
    pub fn advance(&mut self, seconds: f64) -> Result<(), FigureError> {
        for motion in &mut self.motion {
            if *motion != Motion::REST {
                *motion = self.spring.step(*motion, 0.0, seconds)?;
            }
        }
        Ok(())
    }

    /// Whether nothing is still ringing.
    #[must_use]
    pub fn settled(&self) -> bool {
        self.motion.iter().all(|m| m.settled(0.0, SETTLED))
    }

    /// What it is doing to the pose now.
    ///
    /// # Errors
    ///
    /// Cannot fail: every value here came through [`Motion::new`] and is
    /// finite. The result is a `Result` because [`Overlay::add`] is the one
    /// gate on a delta.
    pub fn overlay(&self) -> Result<Overlay, FigureError> {
        let mut overlay = Overlay::NONE;
        for param in Param::ALL {
            let value = self.motion[param.index()].value;
            if value != 0.0 {
                overlay.add(param, value)?;
            }
        }
        Ok(overlay)
    }
}

#[cfg(test)]
#[path = "recoil/tests.rs"]
mod tests;
