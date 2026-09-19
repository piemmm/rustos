//! The damage pipeline, and the healing one beside it.
//!
//! Damage is five ordered steps, each a pure function with its own test:
//! the attacker's power scales the authored blow, defence and resistance
//! reduce it, the defender's statuses modulate what is left, a floor keeps a
//! landed blow from doing nothing, and shields absorb before health does.
//! The order is the whole specification — the same numbers in a different
//! order are a different game — so it is stated once, here, and
//! [`resolve`] is the only composition of it.
//!
//! # Why there is a floor, and why flat defence is safe because of it
//!
//! Armour subtracts flatly, which on its own would let enough armour reduce
//! a small blow to nothing — and a hit that connects and does nothing is
//! indistinguishable from a miss, which reads to a player as a bug rather
//! than as defence. The floor is what makes the flat term safe to have: a
//! blow that lands always takes something. Resistance, by contrast, is
//! proportional and can never reach total, so the two mitigations fail in
//! different directions on purpose.

use crate::bounds::{MAX_BLOW_BASE, MAX_POWER_SCALE_PERMILLE, MIN_LANDED_DAMAGE};
use crate::error::RuleError;
use crate::stat::{Stat, Stats, BASE_POWER_PERMILLE};
use crate::status::StatusSet;

/// What a blow is made of.
///
/// Physical blows scale with [`Stat::Might`]; everything else scales with
/// [`Stat::Insight`]. The mapping is closed and lives with the school, so no
/// authored document can point a sword at a caster's mind.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum School {
    /// Struck, cut, or shot.
    Physical,
    /// Cold.
    Frost,
    /// Fire.
    Flame,
    /// Lightning.
    Storm,
    /// Poison, rot, and disease.
    Blight,
    /// Light.
    Radiant,
}

impl School {
    /// Every school.
    pub const ALL: &'static [Self] = &[
        Self::Physical,
        Self::Frost,
        Self::Flame,
        Self::Storm,
        Self::Blight,
        Self::Radiant,
    ];

    /// The stat a blow of this school scales with.
    #[must_use]
    pub const fn scaling_stat(self) -> Stat {
        match self {
            Self::Physical => Stat::Might,
            Self::Frost | Self::Flame | Self::Storm | Self::Blight | Self::Radiant => Stat::Insight,
        }
    }
}

/// One authored blow, before anybody's stats touch it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Blow {
    school: School,
    base: u32,
    power_scale_permille: u16,
}

impl Blow {
    /// Build a blow.
    ///
    /// `power_scale_permille` is how much of the attacker's power above
    /// unmodified this blow carries: a thousand is full scaling, zero is a
    /// fixed-damage blow whose author did not want a stat in it — a trap, a
    /// fall, an environmental hazard. Values above a thousand scale harder
    /// than the stat alone would.
    ///
    /// # Errors
    ///
    /// [`RuleError::BlowBase`] above [`MAX_BLOW_BASE`], and
    /// [`RuleError::PowerScale`] above [`MAX_POWER_SCALE_PERMILLE`].
    pub const fn new(
        school: School,
        base: u32,
        power_scale_permille: u16,
    ) -> Result<Self, RuleError> {
        if base > MAX_BLOW_BASE {
            return Err(RuleError::BlowBase);
        }
        if power_scale_permille > MAX_POWER_SCALE_PERMILLE {
            return Err(RuleError::PowerScale);
        }
        Ok(Self {
            school,
            base,
            power_scale_permille,
        })
    }

    /// Which school.
    #[must_use]
    pub const fn school(&self) -> School {
        self.school
    }

    /// The authored base damage.
    #[must_use]
    pub const fn base(&self) -> u32 {
        self.base
    }

    /// How much of the attacker's power this blow carries, in parts per
    /// thousand.
    #[must_use]
    pub const fn power_scale_permille(&self) -> u16 {
        self.power_scale_permille
    }
}

/// How a landed blow was paid for.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct Landed {
    /// Taken by shields.
    pub absorbed: u32,
    /// Taken from health.
    pub to_health: u32,
}

impl Landed {
    /// Everything the blow cost the defender.
    #[must_use]
    pub const fn total(&self) -> u32 {
        self.absorbed.saturating_add(self.to_health)
    }
}

/// Step one: the attacker's power scales the authored blow.
#[must_use]
pub fn attacker_scaled(blow: &Blow, attacker: Stats) -> u64 {
    let base = u64::from(blow.base);
    let power = u64::from(attacker.power_permille(blow.school.scaling_stat()));
    let above = power.saturating_sub(u64::from(BASE_POWER_PERMILLE));
    let scaled = base
        .saturating_mul(above)
        .saturating_mul(u64::from(blow.power_scale_permille))
        / 1_000_000;
    base.saturating_add(scaled)
}

/// Step two: armour subtracts, then resistance scales what is left.
///
/// `resistance_permille` comes from the defender's own curve and is strictly
/// below a thousand, so this step cannot reach zero on its own.
#[must_use]
pub fn after_defence(amount: u64, armour: u16, resistance_permille: u32) -> u64 {
    let after_armour = amount.saturating_sub(u64::from(armour));
    let passing = u64::from(1000_u32.saturating_sub(resistance_permille));
    after_armour.saturating_mul(passing) / 1000
}

/// Step three: the defender's statuses modulate what is left.
#[must_use]
pub fn after_status(amount: u64, defender: &StatusSet) -> u64 {
    amount.saturating_mul(u64::from(defender.damage_taken_permille())) / 1000
}

/// Step four: a blow that landed always takes something.
#[must_use]
pub fn floored(amount: u64) -> u32 {
    u32::try_from(amount)
        .unwrap_or(u32::MAX)
        .max(MIN_LANDED_DAMAGE)
}

/// Step five: shields pay before health does.
#[must_use]
pub fn split_absorb(amount: u32, absorb_available: u32) -> Landed {
    let absorbed = amount.min(absorb_available);
    Landed {
        absorbed,
        to_health: amount - absorbed,
    }
}

/// The whole pipeline, in order.
///
/// The only composition of the five steps. A caller that wants a different
/// order wants a different game, and would be changing this function rather
/// than assembling its own.
#[must_use]
pub fn resolve(
    blow: &Blow,
    attacker: Stats,
    defender: Stats,
    armour: u16,
    defender_status: &StatusSet,
) -> Landed {
    let scaled = attacker_scaled(blow, attacker);
    let reduced = after_defence(scaled, armour, defender.resistance_permille());
    let modulated = after_status(reduced, defender_status);
    split_absorb(floored(modulated), defender_status.absorb_available())
}

/// Healing, which is the damage pipeline's shape without its mitigations.
///
/// Armour and resistance have nothing to say about a heal, so the only steps
/// are the healer's power and the target's own statuses — a target under a
/// withering effect receives less. There is no floor: a heal reduced to
/// nothing is a legitimate outcome, where a blow reduced to nothing is not,
/// because a heal that does nothing looks like a heal that was not needed.
#[must_use]
pub fn healed(base: u32, healer: Stats, target_status: &StatusSet) -> u32 {
    let power = u64::from(healer.power_permille(Stat::Insight));
    let scaled = u64::from(base).saturating_mul(power) / 1000;
    let received = scaled.saturating_mul(u64::from(target_status.healing_permille())) / 1000;
    u32::try_from(received).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests;
