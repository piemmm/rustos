//! The always-on idle cycle.
//!
//! A figure standing perfectly still reads as a paused game rather than as a
//! person waiting, and no amount of good artwork fixes it — the eye is
//! looking for the small motion that says the thing is alive. Breathing is
//! that motion: a slow rise and fall of the chest, deliberately too small to
//! read as an animation and impossible to miss when it stops.

use core::f64::consts::TAU;

use tairix_util::mathf;

use crate::error::FigureError;
use crate::pose::{Overlay, Param};
use crate::socket::Side;

/// How much of the chest's motion the shoulders take, against the spine.
///
/// A ribcage widening as it fills carries the arms out with it; nothing but
/// the spine moving reads as a bow rather than a breath.
const SHOULDER_SHARE: f64 = 0.5;

/// A slow cycle that keeps an idle figure alive.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Breath {
    period: f64,
    depth: f64,
    phase: f64,
}

impl Breath {
    /// A breath of `period` seconds at `depth` of the chest's travel.
    ///
    /// `depth` is a fraction of the spine's own range, so it reads the same
    /// on any rig: a few hundredths is a resting breath and a tenth is
    /// exertion.
    ///
    /// # Errors
    ///
    /// [`FigureError::DurationUnreal`] for a period that is not finite and
    /// positive, and [`FigureError::ParamOutsideRange`] for a depth outside
    /// `0..=1`.
    pub fn new(period: f64, depth: f64) -> Result<Self, FigureError> {
        if !period.is_finite() || period <= 0.0 {
            return Err(FigureError::DurationUnreal);
        }
        if !depth.is_finite() || !(0.0..=1.0).contains(&depth) {
            return Err(FigureError::ParamOutsideRange);
        }
        Ok(Self {
            period,
            depth,
            phase: 0.0,
        })
    }

    /// How deep it breathes.
    #[must_use]
    pub const fn depth(self) -> f64 {
        self.depth
    }

    /// Where in the cycle it is, in `0..1`.
    #[must_use]
    pub const fn phase(self) -> f64 {
        self.phase
    }

    /// Advance it by `seconds`.
    ///
    /// # Errors
    ///
    /// [`FigureError::ElapsedUnreal`] for a step that is not finite and
    /// non-negative.
    pub fn advance(&mut self, seconds: f64) -> Result<(), FigureError> {
        if !seconds.is_finite() || seconds < 0.0 {
            return Err(FigureError::ElapsedUnreal);
        }
        let advanced = self.phase + seconds / self.period;
        self.phase = advanced - mathf::floor(advanced);
        Ok(())
    }

    /// What it is doing to the pose now.
    ///
    /// # Errors
    ///
    /// Cannot fail: the deltas are a bounded multiple of a depth this
    /// constructor already accepted. The result is a `Result` because
    /// [`Overlay::add`] is the one gate on a delta.
    pub fn overlay(self) -> Result<Overlay, FigureError> {
        let rise = self.depth * mathf::sin(TAU * self.phase);
        let mut overlay = Overlay::NONE;
        // A filling chest straightens rather than folds, so the bend goes
        // the other way to the rise.
        overlay.add(Param::SpineBend, -rise)?;
        for side in Side::BOTH {
            overlay.add(Param::ShoulderSplay(side), rise * SHOULDER_SHARE)?;
        }
        Ok(overlay)
    }
}

#[cfg(test)]
#[path = "breath/tests.rs"]
mod tests;
