//! The damped spring every settling layer is built from.
//!
//! Recoil, follow-through, and the sway of a cloak or a tail are all one
//! thing: a value pulled toward a target, resisted in proportion to how fast
//! it is moving. Stating it once here is what keeps the snap of an impact and
//! the trail of a hem from drifting into two different curves.
//!
//! # Why the step is solved, not integrated
//!
//! A spring stepped by Euler integration gains energy when the step is long
//! against its own period, so a frame that arrives late makes a cloak flail
//! and a recoil explode — and the frame that arrives late is exactly the one
//! on a loaded machine. This steps by evaluating the oscillator's closed-form
//! solution over the interval instead. The envelope is `e^(-damping * rate *
//! seconds)`, which is at most one for every non-negative step, so the motion
//! cannot grow however long or short the frame was and a stall produces a
//! settled figure rather than a detonated one.

use tairix_util::mathf;

use crate::error::FigureError;

/// Below this the damping ratio is treated as the critical case, where the
/// underdamped and overdamped forms both divide by a vanishing root.
const CRITICAL_BAND: f64 = 1e-4;

/// Where a springing value is and how fast it is going.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Motion {
    /// Where it is.
    pub value: f64,
    /// How fast it is moving, per second.
    pub velocity: f64,
}

impl Motion {
    /// Still, at rest.
    pub const REST: Self = Self {
        value: 0.0,
        velocity: 0.0,
    };

    /// A motion at `value` moving at `velocity`.
    ///
    /// # Errors
    ///
    /// [`FigureError::MotionUnreal`] for a value or a velocity that is not
    /// finite.
    pub fn new(value: f64, velocity: f64) -> Result<Self, FigureError> {
        if !value.is_finite() || !velocity.is_finite() {
            return Err(FigureError::MotionUnreal);
        }
        Ok(Self { value, velocity })
    }

    /// The same motion struck by `impulse`, which adds to its velocity.
    ///
    /// An impulse rather than a displacement because that is what an impact
    /// is: the hand is still where it was on the frame the blow lands, and it
    /// is the speed it picks up that reads as weight.
    ///
    /// # Errors
    ///
    /// [`FigureError::MotionUnreal`] for an impulse that is not finite.
    pub fn struck(self, impulse: f64) -> Result<Self, FigureError> {
        Self::new(self.value, self.velocity + impulse)
    }

    /// Whether it is within `tolerance` of `target` and slower than it.
    #[must_use]
    pub fn settled(self, target: f64, tolerance: f64) -> bool {
        mathf::fabs(self.value - target) <= tolerance && mathf::fabs(self.velocity) <= tolerance
    }
}

/// How a value is pulled back toward its target.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Spring {
    rate: f64,
    damping: f64,
}

impl Spring {
    /// A spring of angular frequency `rate` and damping ratio `damping`.
    ///
    /// `rate` is radians per second, so the undamped period is `TAU / rate`.
    /// `damping` is the ratio to critical: below one overshoots and rings,
    /// one returns as fast as it can without overshooting, above one crawls
    /// back. A follow-through wants under one — the overshoot *is* the
    /// follow-through — and a foot or a settling weapon wants one.
    ///
    /// # Errors
    ///
    /// [`FigureError::SpringUnreal`] for a rate that is not finite and
    /// positive, or a damping ratio that is not finite and non-negative.
    pub fn new(rate: f64, damping: f64) -> Result<Self, FigureError> {
        if !rate.is_finite() || rate <= 0.0 || !damping.is_finite() || damping < 0.0 {
            return Err(FigureError::SpringUnreal);
        }
        Ok(Self { rate, damping })
    }

    /// Its angular frequency, in radians per second.
    #[must_use]
    pub const fn rate(self) -> f64 {
        self.rate
    }

    /// Its damping ratio.
    #[must_use]
    pub const fn damping(self) -> f64 {
        self.damping
    }

    /// `state` after `seconds` pulled toward `target`.
    ///
    /// # Errors
    ///
    /// [`FigureError::ElapsedUnreal`] for a step that is not finite and
    /// non-negative, and [`FigureError::MotionUnreal`] for a target that is
    /// not finite.
    pub fn step(self, state: Motion, target: f64, seconds: f64) -> Result<Motion, FigureError> {
        if !seconds.is_finite() || seconds < 0.0 {
            return Err(FigureError::ElapsedUnreal);
        }
        if !target.is_finite() {
            return Err(FigureError::MotionUnreal);
        }
        if seconds == 0.0 {
            return Ok(state);
        }

        let (offset, velocity) = (state.value - target, state.velocity);
        let (rate, zeta) = (self.rate, self.damping);
        let (settled, speed) = if mathf::fabs(zeta - 1.0) < CRITICAL_BAND {
            // Critically damped: the two roots coincide, so the solution
            // carries a term linear in time rather than a second exponential.
            let decay = mathf::exp(-rate * seconds);
            let slope = velocity + rate * offset;
            let travelled = offset + slope * seconds;
            (travelled * decay, (slope - rate * travelled) * decay)
        } else if zeta < 1.0 {
            let ringing = rate * mathf::sqrt(1.0 - zeta * zeta);
            let decay = mathf::exp(-zeta * rate * seconds);
            let (sin, cos) = (mathf::sin(ringing * seconds), mathf::cos(ringing * seconds));
            let swing = (velocity + zeta * rate * offset) / ringing;
            (
                decay * (offset * cos + swing * sin),
                decay
                    * ((swing * ringing - zeta * rate * offset) * cos
                        - (offset * ringing + zeta * rate * swing) * sin),
            )
        } else {
            let spread = rate * mathf::sqrt(zeta * zeta - 1.0);
            let (fast, slow) = (-zeta * rate - spread, -zeta * rate + spread);
            // The roots differ by `2 * spread`, which the band above keeps
            // away from zero, so this division is safe.
            let slow_part = (velocity - fast * offset) / (slow - fast);
            let fast_part = offset - slow_part;
            let (fast_decay, slow_decay) = (mathf::exp(fast * seconds), mathf::exp(slow * seconds));
            (
                fast_part * fast_decay + slow_part * slow_decay,
                fast * fast_part * fast_decay + slow * slow_part * slow_decay,
            )
        };

        // `exp` saturates rather than overflowing and the envelope never
        // exceeds one, so this is belt-and-braces against a denormal product
        // rather than an expected path — but a `NaN` reaching a pose would
        // be refused frames later and far from its cause.
        Motion::new(target + settled, speed).or(Ok(Motion {
            value: target,
            velocity: 0.0,
        }))
    }
}

#[cfg(test)]
#[path = "spring/tests.rs"]
mod tests;
