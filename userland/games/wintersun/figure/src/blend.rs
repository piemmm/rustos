//! Weighted blending of poses and clips.
//!
//! Weight is accumulated **per parameter**, not per pose, and that is the
//! whole reason masks work: a parameter is weighed only against the clips
//! that had an opinion about it, so a cast playing on the arms does not drag
//! the legs toward rest by the weight it was mixed in at.
//!
//! A parameter nothing wrote resolves to rest. So an overlay covering part of
//! the body is layered *over* a base that covers the rest — added to the same
//! blend — rather than cross-faded against it; cross-fading a full-body clip
//! out from under a partial one would leave the uncovered half at rest as the
//! base's weight reached zero.

use crate::clip::Clip;
use crate::error::FigureError;
use crate::pose::{Mask, Param, Pose};

/// Poses and clips accumulating into one pose.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Blend {
    total: [f64; Param::COUNT],
    weight: [f64; Param::COUNT],
}

impl Blend {
    /// Nothing blended yet.
    pub const EMPTY: Self = Self {
        total: [0.0; Param::COUNT],
        weight: [0.0; Param::COUNT],
    };

    /// Mix `pose` in at `weight`, over the parameters `mask` holds.
    ///
    /// A weight of zero contributes nothing and does not claim a parameter.
    ///
    /// # Errors
    ///
    /// [`FigureError::WeightUnreal`] for a weight that is not finite and
    /// non-negative.
    pub fn add_pose(&mut self, pose: &Pose, mask: Mask, weight: f64) -> Result<(), FigureError> {
        if !weight.is_finite() || weight < 0.0 {
            return Err(FigureError::WeightUnreal);
        }
        if weight == 0.0 {
            return Ok(());
        }
        for param in Param::ALL {
            if mask.holds(param) {
                self.claim(param, pose.get(param), weight);
            }
        }
        Ok(())
    }

    /// Mix `clip`'s pose at `phase` in at `weight`.
    ///
    /// Samples straight into the accumulator, so a blend of several clips
    /// costs no intermediate pose.
    ///
    /// # Errors
    ///
    /// [`FigureError::WeightUnreal`] for a weight that is not finite and
    /// non-negative, and [`FigureError::PhaseOutsideClip`] for a phase
    /// outside `0..=1`.
    pub fn add_clip(&mut self, clip: Clip<'_>, phase: f64, weight: f64) -> Result<(), FigureError> {
        if !weight.is_finite() || weight < 0.0 {
            return Err(FigureError::WeightUnreal);
        }
        if !phase.is_finite() || !(0.0..=1.0).contains(&phase) {
            return Err(FigureError::PhaseOutsideClip);
        }
        if weight == 0.0 {
            return Ok(());
        }
        for curve in clip.curves() {
            self.claim(curve.param(), curve.sample(phase, clip.repeat()), weight);
        }
        Ok(())
    }

    /// The parameters something has written.
    #[must_use]
    pub fn written(&self) -> Mask {
        let mut mask = Mask::NONE;
        for param in Param::ALL {
            if self.weight[param.index()] > 0.0 {
                mask = mask.with(param);
            }
        }
        mask
    }

    /// Whether nothing has been mixed in.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.written().is_empty()
    }

    /// The blended pose: each parameter's weighted mean, and rest for any
    /// parameter nothing claimed.
    ///
    /// # Errors
    ///
    /// Cannot fail for poses and clips this accumulator accepted: a weighted
    /// mean of values inside a range is inside it. The result is a `Result`
    /// because [`Pose::set`] is the one gate on a parameter's value and the
    /// crate keeps no second way past it.
    pub fn resolve(&self) -> Result<Pose, FigureError> {
        let mut pose = Pose::REST;
        for param in Param::ALL {
            let weight = self.weight[param.index()];
            if weight > 0.0 {
                let mean = self.total[param.index()] / weight;
                pose.set(param, param.range().clamp(mean))?;
            }
        }
        Ok(pose)
    }

    fn claim(&mut self, param: Param, value: f64, weight: f64) {
        self.total[param.index()] += value * weight;
        self.weight[param.index()] += weight;
    }
}

impl Default for Blend {
    fn default() -> Self {
        Self::EMPTY
    }
}

#[cfg(test)]
#[path = "blend/tests.rs"]
mod tests;
