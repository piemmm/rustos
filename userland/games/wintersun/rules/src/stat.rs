//! The closed stat set, and the curves everything else is derived from.
//!
//! Five stats, and no sixth without a rule that needs one. Every derived
//! quantity — health, the resource pool, movement speed, offensive power,
//! resistance — is a stated function of them, evaluated in integer
//! arithmetic over a bounded domain. That is what makes the numbers
//! reviewable: a curve is a line of arithmetic with a test at both ends and
//! a monotonicity test across the whole domain, not a table somebody tuned.
//!
//! # Why resistance cannot reach total
//!
//! Resistance is `stat / (stat + half)`, which approaches one and never
//! arrives. So no stat value makes a target immune, and the property holds
//! because of the curve's shape rather than a clamp bolted onto it — there
//! is no value to get wrong, and nothing for a later change to remove.

use crate::bounds::{MAX_SPEED_SUB_UNITS_PER_TICK, MAX_STAT, RESIST_HALF_STAT};
use crate::error::RuleError;

/// Health at zero [`Stat::Vitality`].
pub const BASE_HEALTH: u32 = 100;

/// Health each point of [`Stat::Vitality`] adds.
pub const HEALTH_PER_VITALITY: u32 = 10;

/// Resource at zero [`Stat::Resolve`].
pub const BASE_RESOURCE: u32 = 50;

/// Resource each point of [`Stat::Resolve`] adds.
pub const RESOURCE_PER_RESOLVE: u32 = 5;

/// Movement speed at zero [`Stat::Agility`], in sub-units per tick.
///
/// About three and a half world units a second at the default rate: a walk.
pub const BASE_SPEED_SUB_UNITS: i32 = 120;

/// Movement speed [`MAX_STAT`] of [`Stat::Agility`] adds, in sub-units per
/// tick.
///
/// So a maximally agile body moves two and a half times a sedentary one, and
/// the fastest speed any curve produces stays far below the arithmetic
/// ceiling the broad phase is built against.
pub const SPEED_SPAN_SUB_UNITS: i32 = 180;

/// Offensive power each point of the scaling stat adds, in parts per
/// thousand.
pub const POWER_PER_POINT_PERMILLE: u32 = 5;

/// Power at zero of the scaling stat, in parts per thousand — an unmodified
/// blow.
pub const BASE_POWER_PERMILLE: u32 = 1000;

/// One of the five.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Stat {
    /// Physical power.
    Might,
    /// Movement speed.
    Agility,
    /// Health.
    Vitality,
    /// Power of everything that is not a physical blow.
    Insight,
    /// The resource pool, and resistance.
    Resolve,
}

impl Stat {
    /// Every stat, in the order the digest folds them.
    pub const ALL: &'static [Self] = &[
        Self::Might,
        Self::Agility,
        Self::Vitality,
        Self::Insight,
        Self::Resolve,
    ];
}

/// One entity's five stats.
///
/// No public fields: every value is inside [`MAX_STAT`] because the
/// constructor checked it, so every curve below is evaluated on its stated
/// domain and never extrapolated past it.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct Stats {
    might: u16,
    agility: u16,
    vitality: u16,
    insight: u16,
    resolve: u16,
}

impl Stats {
    /// Build a stat block.
    ///
    /// # Errors
    ///
    /// [`RuleError::Stat`] when any value exceeds [`MAX_STAT`].
    pub const fn new(
        might: u16,
        agility: u16,
        vitality: u16,
        insight: u16,
        resolve: u16,
    ) -> Result<Self, RuleError> {
        if might > MAX_STAT
            || agility > MAX_STAT
            || vitality > MAX_STAT
            || insight > MAX_STAT
            || resolve > MAX_STAT
        {
            return Err(RuleError::Stat);
        }
        Ok(Self {
            might,
            agility,
            vitality,
            insight,
            resolve,
        })
    }

    /// The value of one stat.
    #[must_use]
    pub const fn get(self, stat: Stat) -> u16 {
        match stat {
            Stat::Might => self.might,
            Stat::Agility => self.agility,
            Stat::Vitality => self.vitality,
            Stat::Insight => self.insight,
            Stat::Resolve => self.resolve,
        }
    }

    /// Maximum health.
    #[must_use]
    pub fn max_health(self) -> u32 {
        BASE_HEALTH.saturating_add(u32::from(self.vitality).saturating_mul(HEALTH_PER_VITALITY))
    }

    /// Maximum resource.
    #[must_use]
    pub fn max_resource(self) -> u32 {
        BASE_RESOURCE.saturating_add(u32::from(self.resolve).saturating_mul(RESOURCE_PER_RESOLVE))
    }

    /// Unimpeded movement speed, in sub-units per tick.
    ///
    /// The status set scales this; nothing else does.
    #[must_use]
    pub fn speed_sub_units_per_tick(self) -> i32 {
        let span =
            i32::from(self.agility).saturating_mul(SPEED_SPAN_SUB_UNITS) / i32::from(MAX_STAT);
        BASE_SPEED_SUB_UNITS.saturating_add(span)
    }

    /// How strongly this block scales a blow that scales with `stat`, in
    /// parts per thousand.
    #[must_use]
    pub fn power_permille(self, stat: Stat) -> u32 {
        BASE_POWER_PERMILLE
            .saturating_add(u32::from(self.get(stat)).saturating_mul(POWER_PER_POINT_PERMILLE))
    }

    /// Proportional damage mitigation, in parts per thousand.
    ///
    /// Strictly below a thousand at every legal stat, so resistance alone
    /// never negates a blow.
    #[must_use]
    pub fn resistance_permille(self) -> u32 {
        let stat = u32::from(self.resolve);
        stat.saturating_mul(1000) / (stat + RESIST_HALF_STAT)
    }
}

/// Assert at compile time that the fastest curve output is inside the
/// arithmetic bound the broad phase and the movement integrator assume.
const _: () = assert!(
    BASE_SPEED_SUB_UNITS + SPEED_SPAN_SUB_UNITS <= MAX_SPEED_SUB_UNITS_PER_TICK,
    "the speed curve must not exceed the movement bound"
);

#[cfg(test)]
mod tests;
