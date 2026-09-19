//! What the simulation advances: one body, its numbers, and its state.
//!
//! An entity is the whole of what a tick reads and writes. Nothing about it
//! is a client's to assert: the position is where the realm put it, the
//! health is what the realm's pipeline left, and the held direction is the
//! last movement intent the realm *admitted* — not the last one sent.
//!
//! # The carried remainder, and why movement needs one
//!
//! Speed is sub-units per tick and a held direction is a fraction of it, so
//! a step is a division — and a division that truncated every tick would
//! quietly make diagonal movement slower than cardinal, and a slow walk
//! slower still, in a way a player feels and cannot name. Each body
//! therefore carries the fractional part of its own step forward, so
//! displacement accumulates exactly. It is integer bookkeeping, so it costs
//! two words and nothing in determinism.

use tairix_wintersun_net::value::{
    Direction, EntityId, EntityKind, EntityState, Facing, WorldPoint, WorldVector,
};

use crate::bounds::{MAX_ARMOUR, MAX_BODY_RADIUS_SUB_UNITS};
use crate::error::RuleError;
use crate::pool::Pool;
use crate::stat::Stats;
use crate::status::StatusSet;

/// The fixed-point divisions of a sub-unit the carried remainder counts in.
///
/// A held direction is authored in the wire's own Q1.15, so the remainder
/// shares its scale: a step is one multiply, one shift and one mask, with
/// nothing rounded away.
pub const RESIDUE_SCALE: i64 = 1 << 15;

/// What a body needs to exist.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SpawnSpec {
    kind: EntityKind,
    at: WorldPoint,
    stats: Stats,
    armour: u16,
    radius: u16,
}

impl SpawnSpec {
    /// Describe a body to spawn.
    ///
    /// # Errors
    ///
    /// [`RuleError::BodyRadius`] for a zero radius or one above
    /// [`MAX_BODY_RADIUS_SUB_UNITS`], and [`RuleError::Armour`] above
    /// [`MAX_ARMOUR`].
    pub fn new(
        kind: EntityKind,
        at: WorldPoint,
        stats: Stats,
        armour: u16,
        radius: u16,
    ) -> Result<Self, RuleError> {
        if radius == 0 || i32::from(radius) > MAX_BODY_RADIUS_SUB_UNITS {
            return Err(RuleError::BodyRadius);
        }
        if armour > MAX_ARMOUR {
            return Err(RuleError::Armour);
        }
        Ok(Self {
            kind,
            at,
            stats,
            armour,
            radius,
        })
    }

    /// What it is.
    #[must_use]
    pub const fn kind(&self) -> EntityKind {
        self.kind
    }

    /// Where it starts.
    #[must_use]
    pub const fn at(&self) -> WorldPoint {
        self.at
    }

    /// Its stats.
    #[must_use]
    pub const fn stats(&self) -> Stats {
        self.stats
    }
}

/// One body in the simulation.
#[derive(Clone, Debug)]
pub struct Entity {
    id: EntityId,
    kind: EntityKind,
    at: WorldPoint,
    facing: Facing,
    radius: u16,
    armour: u16,
    stats: Stats,
    health: Pool,
    resource: Pool,
    status: StatusSet,
    held: Direction,
    motion: WorldVector,
    residue_x: i64,
    residue_y: i64,
    last_sequence: u64,
    intents_this_tick: u8,
}

impl Entity {
    /// Bring a body into being at full health and resource.
    pub(crate) fn spawn(id: EntityId, spec: SpawnSpec) -> Self {
        Self {
            id,
            kind: spec.kind,
            at: spec.at,
            facing: Facing(0),
            radius: spec.radius,
            armour: spec.armour,
            stats: spec.stats,
            health: Pool::full(spec.stats.max_health()),
            resource: Pool::full(spec.stats.max_resource()),
            status: StatusSet::new(),
            held: Direction::still(),
            motion: WorldVector::default(),
            residue_x: 0,
            residue_y: 0,
            last_sequence: 0,
            intents_this_tick: 0,
        }
    }

    /// Its identity within the zone.
    #[must_use]
    pub const fn id(&self) -> EntityId {
        self.id
    }

    /// What it is.
    #[must_use]
    pub const fn kind(&self) -> EntityKind {
        self.kind
    }

    /// Where it is.
    #[must_use]
    pub const fn at(&self) -> WorldPoint {
        self.at
    }

    /// Which way it faces.
    #[must_use]
    pub const fn facing(&self) -> Facing {
        self.facing
    }

    /// Its body radius, in sub-units.
    #[must_use]
    pub const fn radius(&self) -> u16 {
        self.radius
    }

    /// Its flat damage reduction.
    #[must_use]
    pub const fn armour(&self) -> u16 {
        self.armour
    }

    /// Its stats.
    #[must_use]
    pub const fn stats(&self) -> Stats {
        self.stats
    }

    /// Its health.
    #[must_use]
    pub const fn health(&self) -> Pool {
        self.health
    }

    /// Its resource.
    #[must_use]
    pub const fn resource(&self) -> Pool {
        self.resource
    }

    /// What it is carrying.
    #[must_use]
    pub const fn status(&self) -> &StatusSet {
        &self.status
    }

    /// The last movement direction the realm admitted.
    #[must_use]
    pub const fn held(&self) -> Direction {
        self.held
    }

    /// How far it moved on the last step, for a client to extrapolate from.
    #[must_use]
    pub const fn motion(&self) -> WorldVector {
        self.motion
    }

    /// Whether it is still alive.
    #[must_use]
    pub const fn is_alive(&self) -> bool {
        !self.health.is_empty()
    }

    /// The view of it a client is sent.
    #[must_use]
    pub const fn state(&self) -> EntityState {
        EntityState {
            id: self.id,
            kind: self.kind,
            at: self.at,
            motion: self.motion,
            facing: self.facing,
        }
    }

    /// The last intent sequence applied, which the realm echoes so the
    /// client knows what to stop replaying.
    #[must_use]
    pub const fn acknowledged(&self) -> u64 {
        self.last_sequence
    }

    /// The carried fractional step, in [`RESIDUE_SCALE`] divisions of a
    /// sub-unit.
    #[must_use]
    pub const fn residue(&self) -> (i64, i64) {
        (self.residue_x, self.residue_y)
    }

    pub(crate) fn status_mut(&mut self) -> &mut StatusSet {
        &mut self.status
    }

    pub(crate) fn health_mut(&mut self) -> &mut Pool {
        &mut self.health
    }

    pub(crate) fn resource_mut(&mut self) -> &mut Pool {
        &mut self.resource
    }

    /// Record an admitted movement intent, turning to face it.
    ///
    /// The direction is recorded whatever the body's statuses say, because a
    /// root or a stun that refused the input would leave a stale direction
    /// to resume the moment it expired — a body that changed its mind while
    /// held would walk the old way. The statuses take effect through the
    /// speed instead, and a body that cannot move does not turn either.
    ///
    /// A body holding nothing keeps the heading it had: facing nowhere is
    /// not a direction, and spinning to a default on release would be a
    /// visible twitch.
    pub(crate) fn hold(&mut self, direction: Direction) {
        self.held = direction;
        if !self.status.may_move() {
            return;
        }
        if let Some(facing) = Facing::towards(i32::from(direction.x()), i32::from(direction.y())) {
            self.facing = facing;
        }
    }

    /// Record the outcome of one movement step.
    pub(crate) fn place(&mut self, at: WorldPoint, residue: (i64, i64), moved: WorldVector) {
        self.at = at;
        self.residue_x = residue.0;
        self.residue_y = residue.1;
        self.motion = moved;
    }

    /// Note that an intent of this sequence was applied.
    pub(crate) fn acknowledge(&mut self, sequence: u64) {
        self.last_sequence = sequence;
        self.intents_this_tick = self.intents_this_tick.saturating_add(1);
    }

    /// How many intents have been admitted this tick.
    pub(crate) const fn admitted_this_tick(&self) -> u8 {
        self.intents_this_tick
    }

    /// Start a fresh tick's intent budget.
    pub(crate) fn reset_tick_budget(&mut self) {
        self.intents_this_tick = 0;
    }
}

#[cfg(test)]
mod tests;
