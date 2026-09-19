//! The status vocabulary, and the rules that govern how it stacks.
//!
//! Eleven kinds, closed. Each has a magnitude whose meaning its kind fixes,
//! a duration in ticks the realm counted, and the entity that applied it.
//! Adding a twelfth is adding a variant, a rule and a test — there is no
//! scripting seam here and no per-status code path.
//!
//! # The three rules that stop a status set being exploitable
//!
//! * **Proportional effects take the strongest, never the sum.** Two slows
//!   that added would exceed a full stop; two mitigations that added would
//!   reach total immunity. Resource-shaped effects — a bleed, a regeneration,
//!   an absorb pool — do sum, because summing is what they mean.
//! * **The set is partitioned into harmful and helpful, each with its own
//!   ceiling.** One shared ceiling would let a player fill it with
//!   self-applied buffs and become unstunnable, which is a defence nobody
//!   granted. Crowding a partition can therefore only displace an effect of
//!   the same sign, and only one that the arrival outlasts.
//! * **Losing control diminishes; taking damage does not.** Repeated stuns,
//!   roots, silences and slows are what make a game unplayable rather than
//!   hard, so each application within the window is worth half the last and
//!   the fourth is refused. A bleed is not control and does not diminish.

use tairix_inline::ArrayVec;
use tairix_wintersun_net::value::EntityId;

use crate::bounds::{
    DIMINISH_IMMUNE_AT, MAX_ABSORB, MAX_HARMFUL_STATUS, MAX_HELPFUL_STATUS, MAX_PERIODIC_HEALTH,
    MAX_PERMILLE_MAGNITUDE, MAX_STATUS_PER_ENTITY, MAX_STATUS_TICKS,
};
use crate::error::RuleError;

/// What a kind's magnitude counts.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Magnitude {
    /// Nothing: the effect is binary and the magnitude must be zero.
    None,
    /// Parts per thousand, at most [`MAX_PERMILLE_MAGNITUDE`].
    Permille,
    /// Health a tick, at most [`MAX_PERIODIC_HEALTH`].
    PerTickHealth,
    /// Damage the effect will absorb, at most [`MAX_ABSORB`].
    Absorb,
}

/// One of the eleven.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum StatusKind {
    /// Moves more slowly.
    Slow,
    /// Cannot move. May still act.
    Root,
    /// Cannot move or act at all.
    Stun,
    /// Cannot cast. May still move and act.
    Silence,
    /// Loses health every tick.
    Bleed,
    /// Takes more damage.
    Vulnerable,
    /// Receives less healing.
    Wither,
    /// Moves more quickly.
    Haste,
    /// Gains health every tick.
    Regenerate,
    /// Absorbs damage until the pool is spent.
    Shield,
    /// Takes less damage.
    Fortify,
}

impl StatusKind {
    /// Every kind, in the order the diminishing ledger and the digest index
    /// them.
    pub const ALL: &'static [Self] = &[
        Self::Slow,
        Self::Root,
        Self::Stun,
        Self::Silence,
        Self::Bleed,
        Self::Vulnerable,
        Self::Wither,
        Self::Haste,
        Self::Regenerate,
        Self::Shield,
        Self::Fortify,
    ];

    /// How many kinds there are.
    pub const COUNT: usize = Self::ALL.len();

    /// This kind's position in [`Self::ALL`].
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Slow => 0,
            Self::Root => 1,
            Self::Stun => 2,
            Self::Silence => 3,
            Self::Bleed => 4,
            Self::Vulnerable => 5,
            Self::Wither => 6,
            Self::Haste => 7,
            Self::Regenerate => 8,
            Self::Shield => 9,
            Self::Fortify => 10,
        }
    }

    /// Whether this kind works against the entity carrying it.
    #[must_use]
    pub const fn is_harmful(self) -> bool {
        matches!(
            self,
            Self::Slow
                | Self::Root
                | Self::Stun
                | Self::Silence
                | Self::Bleed
                | Self::Vulnerable
                | Self::Wither
        )
    }

    /// Whether repeated application within the window is worth less each
    /// time.
    ///
    /// The control effects, and only those: being stunned repeatedly is what
    /// stops a fight being a fight, where bleeding repeatedly is just
    /// damage.
    #[must_use]
    pub const fn diminishes(self) -> bool {
        matches!(self, Self::Slow | Self::Root | Self::Stun | Self::Silence)
    }

    /// What this kind's magnitude counts.
    #[must_use]
    pub const fn magnitude(self) -> Magnitude {
        match self {
            Self::Root | Self::Stun | Self::Silence => Magnitude::None,
            Self::Slow | Self::Vulnerable | Self::Wither | Self::Haste | Self::Fortify => {
                Magnitude::Permille
            }
            Self::Bleed | Self::Regenerate => Magnitude::PerTickHealth,
            Self::Shield => Magnitude::Absorb,
        }
    }

    /// The largest magnitude this kind admits.
    #[must_use]
    pub const fn magnitude_ceiling(self) -> u16 {
        match self.magnitude() {
            Magnitude::None => 0,
            Magnitude::Permille => MAX_PERMILLE_MAGNITUDE,
            Magnitude::PerTickHealth => MAX_PERIODIC_HEALTH,
            Magnitude::Absorb => MAX_ABSORB,
        }
    }
}

/// One status on one entity.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Status {
    kind: StatusKind,
    magnitude: u16,
    remaining: u32,
    source: EntityId,
}

impl Status {
    /// Build a status.
    ///
    /// # Errors
    ///
    /// [`RuleError::StatusMagnitude`] for a magnitude outside its kind's
    /// range — including any non-zero magnitude on a binary kind — and
    /// [`RuleError::StatusDuration`] for a zero or over-long duration.
    pub const fn new(
        kind: StatusKind,
        magnitude: u16,
        remaining: u32,
        source: EntityId,
    ) -> Result<Self, RuleError> {
        if magnitude > kind.magnitude_ceiling() {
            return Err(RuleError::StatusMagnitude);
        }
        if remaining == 0 || remaining > MAX_STATUS_TICKS {
            return Err(RuleError::StatusDuration);
        }
        Ok(Self {
            kind,
            magnitude,
            remaining,
            source,
        })
    }

    /// Which kind.
    #[must_use]
    pub const fn kind(&self) -> StatusKind {
        self.kind
    }

    /// The magnitude, in its kind's units.
    #[must_use]
    pub const fn magnitude(&self) -> u16 {
        self.magnitude
    }

    /// Ticks left before it expires.
    #[must_use]
    pub const fn remaining(&self) -> u32 {
        self.remaining
    }

    /// Who applied it.
    #[must_use]
    pub const fn source(&self) -> EntityId {
        self.source
    }
}

/// What happened to an application.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Application {
    /// Admitted for the duration asked for.
    Applied,
    /// Admitted, but this application contributed only this many ticks
    /// rather than the duration it asked for.
    Diminished(u32),
    /// Refused: the target is immune to this kind until its window resets.
    Immune,
    /// Refused: the target's partition is full of effects that all outlast
    /// this one.
    Crowded,
}

/// How often one kind has been applied recently, and how long since.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
struct Diminish {
    applications: u8,
    idle: u32,
}

/// Every status on one entity, with its diminishing ledger.
///
/// Held inline: the ceiling is a containment bound on how much one entity
/// can be made to carry, not a capacity that should follow the machine, so
/// this allocates nothing.
#[derive(Clone, Debug)]
pub struct StatusSet {
    held: ArrayVec<Status, MAX_STATUS_PER_ENTITY>,
    diminish: [Diminish; StatusKind::COUNT],
}

impl Default for StatusSet {
    fn default() -> Self {
        Self::new()
    }
}

impl StatusSet {
    /// An entity carrying nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            held: ArrayVec::new(),
            diminish: [Diminish {
                applications: 0,
                idle: 0,
            }; StatusKind::COUNT],
        }
    }

    /// Every status held, in application order.
    #[must_use]
    pub fn held(&self) -> &[Status] {
        self.held.as_slice()
    }

    /// Whether any status of `kind` is held.
    #[must_use]
    pub fn has(&self, kind: StatusKind) -> bool {
        self.held.iter().any(|status| status.kind == kind)
    }

    /// Apply a status, resolving diminishing returns and crowding.
    pub fn apply(&mut self, status: Status) -> Application {
        let mut admitted = status;
        let mut outcome = Application::Applied;

        if status.kind.diminishes() {
            let ledger = *self.ledger(status.kind);
            if ledger.applications >= DIMINISH_IMMUNE_AT {
                // Still refused, and the refusal itself counts as contact:
                // otherwise an attacker could hold immunity open forever by
                // applying into it, and the window would never reset.
                self.ledger(status.kind).idle = 0;
                return Application::Immune;
            }
            let scaled = (status.remaining >> u32::from(ledger.applications)).max(1);
            if scaled < status.remaining {
                outcome = Application::Diminished(scaled);
            }
            admitted.remaining = scaled;
        }

        let result = self.admit(admitted, outcome);
        // The chain counts applications that *landed*. Counting a refused
        // one would let a body with a full harmful partition be walked up
        // to immunity by effects it never carried.
        if admitted.kind.diminishes() && result != Application::Crowded {
            let ledger = self.ledger(admitted.kind);
            ledger.applications = ledger.applications.saturating_add(1);
            ledger.idle = 0;
        }
        result
    }

    /// Refresh, insert, or displace — the partition half of an application.
    fn admit(&mut self, admitted: Status, outcome: Application) -> Application {
        if let Some(existing) = self
            .held
            .as_mut_slice()
            .iter_mut()
            .find(|held| held.kind == admitted.kind && held.source == admitted.source)
        {
            // A weaker or shorter re-application must not undo a stronger
            // one: refreshing takes the better of each.
            existing.remaining = existing.remaining.max(admitted.remaining);
            existing.magnitude = existing.magnitude.max(admitted.magnitude);
            return outcome;
        }

        let partition_bound = if admitted.kind.is_harmful() {
            MAX_HARMFUL_STATUS
        } else {
            MAX_HELPFUL_STATUS
        };
        let occupancy = self
            .held
            .iter()
            .filter(|held| held.kind.is_harmful() == admitted.kind.is_harmful())
            .count();

        if occupancy < partition_bound {
            if self.held.try_push(admitted).is_ok() {
                return outcome;
            }
            return Application::Crowded;
        }

        // A full partition yields only to an arrival that outlasts its
        // shortest holder, so nothing already carried is ever weakened. A
        // body can therefore hold off a further effect *of the same sign*
        // by carrying enough long ones — a trade, not a defence, and the
        // reason the two signs do not share a ceiling.
        let weakest = self
            .held
            .iter()
            .enumerate()
            .filter(|(_, held)| held.kind.is_harmful() == admitted.kind.is_harmful())
            .map(|(index, held)| (held.remaining, index))
            .min();
        match weakest {
            Some((remaining, index)) if admitted.remaining > remaining => {
                match self.held.as_mut_slice().get_mut(index) {
                    Some(slot) => {
                        *slot = admitted;
                        outcome
                    }
                    None => Application::Crowded,
                }
            }
            _ => Application::Crowded,
        }
    }

    /// One kind's diminishing ledger.
    ///
    /// The index is a total map onto `0..COUNT` and the array is that long,
    /// so the clamp only makes the bound visible to a reader.
    fn ledger(&mut self, kind: StatusKind) -> &mut Diminish {
        &mut self.diminish[kind.index().min(StatusKind::COUNT - 1)]
    }

    /// Remove every status of `kind`, returning how many went.
    pub fn clear_kind(&mut self, kind: StatusKind) -> usize {
        let before = self.held.len();
        self.held.retain(|status| status.kind != kind);
        before - self.held.len()
    }

    /// Advance one tick: expire what has run out, and age the diminishing
    /// ledger.
    ///
    /// Returns how many statuses expired. Call it *after* reading the
    /// periodic effects, so a one-tick status ticks once.
    pub fn advance(&mut self, reset_ticks: u32) -> usize {
        for status in self.held.as_mut_slice() {
            status.remaining = status.remaining.saturating_sub(1);
        }
        let before = self.held.len();
        self.held.retain(|status| status.remaining > 0);

        for ledger in &mut self.diminish {
            if ledger.applications == 0 {
                continue;
            }
            ledger.idle = ledger.idle.saturating_add(1);
            if ledger.idle >= reset_ticks {
                *ledger = Diminish::default();
            }
        }
        before - self.held.len()
    }

    /// Movement speed as a fraction of unimpeded, in parts per thousand.
    ///
    /// Zero while stunned or rooted. Otherwise the strongest haste less the
    /// strongest slow, which cannot reach zero because no proportional
    /// magnitude reaches a full thousand.
    #[must_use]
    pub fn speed_permille(&self) -> u32 {
        if self.has(StatusKind::Stun) || self.has(StatusKind::Root) {
            return 0;
        }
        let haste = u32::from(self.strongest(StatusKind::Haste));
        let slow = u32::from(self.strongest(StatusKind::Slow));
        (1000 + haste).saturating_sub(slow)
    }

    /// Whether the entity may act at all.
    #[must_use]
    pub fn may_act(&self) -> bool {
        !self.has(StatusKind::Stun)
    }

    /// Whether the entity may cast.
    #[must_use]
    pub fn may_cast(&self) -> bool {
        self.may_act() && !self.has(StatusKind::Silence)
    }

    /// Whether the entity may move.
    #[must_use]
    pub fn may_move(&self) -> bool {
        self.speed_permille() > 0
    }

    /// Damage taken as a fraction of incoming, in parts per thousand.
    #[must_use]
    pub fn damage_taken_permille(&self) -> u32 {
        let up = u32::from(self.strongest(StatusKind::Vulnerable));
        let down = u32::from(self.strongest(StatusKind::Fortify));
        (1000 + up).saturating_sub(down)
    }

    /// Healing received as a fraction of incoming, in parts per thousand.
    #[must_use]
    pub fn healing_permille(&self) -> u32 {
        1000_u32.saturating_sub(u32::from(self.strongest(StatusKind::Wither)))
    }

    /// Damage the shields could absorb right now.
    #[must_use]
    pub fn absorb_available(&self) -> u32 {
        self.held
            .iter()
            .filter(|status| status.kind == StatusKind::Shield)
            .fold(0_u32, |total, status| {
                total.saturating_add(u32::from(status.magnitude))
            })
    }

    /// Spend up to `amount` from the shields, returning what was absorbed.
    ///
    /// Shortest-lived first, so a shield about to expire is spent before one
    /// that will still be there — the order that wastes the least, and a
    /// stated one rather than whatever the set happened to hold.
    pub fn consume_absorb(&mut self, amount: u32) -> u32 {
        let mut left = amount;
        let mut absorbed = 0_u32;
        while left > 0 {
            let next = self
                .held
                .iter()
                .enumerate()
                .filter(|(_, status)| status.kind == StatusKind::Shield && status.magnitude > 0)
                .map(|(index, status)| (status.remaining, index))
                .min();
            let Some((_, index)) = next else { break };
            let Some(shield) = self.held.as_mut_slice().get_mut(index) else {
                break;
            };
            let take = u16::try_from(left)
                .unwrap_or(u16::MAX)
                .min(shield.magnitude);
            shield.magnitude -= take;
            left -= u32::from(take);
            absorbed = absorbed.saturating_add(u32::from(take));
        }
        self.held
            .retain(|status| status.kind != StatusKind::Shield || status.magnitude > 0);
        absorbed
    }

    /// Health the periodic statuses change this tick: positive heals,
    /// negative harms.
    #[must_use]
    pub fn periodic_health(&self) -> i64 {
        self.held.iter().fold(0_i64, |total, status| {
            let magnitude = i64::from(status.magnitude);
            match status.kind {
                StatusKind::Regenerate => total + magnitude,
                StatusKind::Bleed => total - magnitude,
                _ => total,
            }
        })
    }

    /// The largest magnitude held of one kind, or zero.
    fn strongest(&self, kind: StatusKind) -> u16 {
        self.held
            .iter()
            .filter(|status| status.kind == kind)
            .map(Status::magnitude)
            .max()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests;
