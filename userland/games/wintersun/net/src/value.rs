//! The values messages are built from: identifiers, world geometry, entity
//! state, stored world edits, and play events.
//!
//! Identifiers are newtypes rather than bare integers so a character id
//! cannot be passed where an entity id belongs. The *meaning* of a kind, an
//! action, a spell, or an item is the realm's content, not this crate's: the
//! wire carries and bounds the number, and the simulation resolves it.
//!
//! Geometry is fixed point, not floating point. The authoritative simulation
//! is `f64`, but a wire value must be bit-identical on every target and must
//! not carry a NaN or an infinity a decoder would then have to special-case.
//! [`WorldPoint`] therefore counts sub-units, and the sub-unit is a power of
//! two so the conversion is exact in both directions.

use tairix_util::mathf;

use crate::bounds::{
    ENTITY_ID_LEN, ENTITY_STATE_LEN, GAME_EVENT_LEN, MAX_DIRECTION_MAGNITUDE_SQ,
    PLAY_EVENT_PAYLOAD_LEN, WORLD_CHANGE_PAYLOAD_LEN, WORLD_EDIT_LEN,
};
use crate::codec::{Reader, WireItem, Writer};
use crate::error::WireError;

/// A realm account.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct AccountId(pub u64);

/// A character belonging to an account.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct CharacterId(pub u64);

/// A live entity in a zone.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct EntityId(pub u64);

/// What an entity *is*, resolved against the realm's content to a figure
/// preset and a behaviour. Opaque here.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct EntityKind(pub u16);

/// An action in the realm's action table.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ActionId(pub u16);

/// A spell in the realm's spell documents.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SpellId(pub u16);

/// An item kind in the realm's item documents.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ItemId(pub u32);

/// An inventory or equipment slot on a character.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SlotIndex(pub u16);

/// A placed structure in the world.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct StructureId(pub u32);

/// A gatherable resource node in the world.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ResourceNodeId(pub u32);

/// A position in the world, in sub-units.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct WorldPoint {
    /// Eastward sub-units from the realm origin.
    pub x: i32,
    /// Southward sub-units from the realm origin.
    pub y: i32,
}

/// A velocity, in sub-units per authoritative tick.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct WorldVector {
    /// Eastward component.
    pub x: i16,
    /// Southward component.
    pub y: i16,
}

/// A chunk of the generated world.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ChunkCoord {
    /// Eastward chunk index.
    pub x: i32,
    /// Southward chunk index.
    pub y: i32,
}

/// A heading, as a fraction of a full turn.
///
/// Every `u16` is a valid heading, so no heading can be out of range and no
/// decoder has to check one.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default, Ord, PartialOrd)]
pub struct Facing(pub u16);

impl Facing {
    /// This heading as a unit vector on the world axes.
    ///
    /// Zero is east and the turn advances toward south, which is the sense
    /// [`WorldPoint`]'s axes already have. Stated here, with the type,
    /// because a heading and a position disagreeing about which way round
    /// the world is would be a defect no single crate could see.
    #[must_use]
    pub fn unit_vector(self) -> (f64, f64) {
        let turn = f64::from(self.0) / f64::from(TURN_UNITS);
        let radians = turn * core::f64::consts::TAU;
        (mathf::cos(radians), mathf::sin(radians))
    }

    /// The heading pointing along `(x, y)`, or `None` for the zero vector,
    /// which points nowhere.
    ///
    /// The exact inverse of [`Self::unit_vector`]'s convention, and here
    /// beside it for the same reason: a simulation deriving a heading from a
    /// movement and a renderer deriving a movement from a heading must agree
    /// which way round the world is, and two crates each picking a sense
    /// would be a defect neither could see.
    #[must_use]
    pub fn towards(x: i32, y: i32) -> Option<Self> {
        if x == 0 && y == 0 {
            return None;
        }
        let radians = mathf::atan2(f64::from(y), f64::from(x));
        let turn = radians / core::f64::consts::TAU;
        let units = mathf::round_i32(turn * f64::from(TURN_UNITS)).rem_euclid(TURN_UNITS);
        // `rem_euclid` by the turn leaves a value inside `u16`, so the
        // narrowing is exact.
        u16::try_from(units).ok().map(Self)
    }
}

/// Divisions of a full turn a [`Facing`] counts in — every `u16`.
const TURN_UNITS: i32 = 1 << 16;

/// A position *within* an authoritative tick, as a fraction of it.
///
/// This is the sub-tick sample time an intent carries, so the realm places an
/// action inside the tick it arrived in rather than snapping it to the
/// boundary. Expressed as a fraction rather than a duration because a
/// fraction cannot exceed the tick, whatever rate the realm runs at, and
/// therefore has no invalid value to reject.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default, Ord, PartialOrd)]
pub struct TickPhase(pub u16);

/// An instant in the realm's own clock: a tick and a phase within it.
///
/// The realm's monotonic clock is the only clock. A client reports one of
/// these as the moment it *saw* what it is shooting at; the realm validates
/// and clamps it against the peer's measured round trip before rewinding, so
/// a client that reports an old view time is describing a window the realm
/// will not honour.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default, Ord, PartialOrd)]
pub struct TickInstant {
    /// The authoritative tick.
    pub tick: u64,
    /// The phase within it.
    pub phase: TickPhase,
}

impl TickInstant {
    /// Read one instant.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] on a short read.
    pub fn read(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Ok(Self {
            tick: r.u64()?,
            phase: TickPhase(r.u16()?),
        })
    }

    /// Write one instant.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when the output is full.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), WireError> {
        w.u64(self.tick)?;
        w.u16(self.phase.0)
    }
}

/// A held movement direction, as a Q1.15 vector no longer than one unit.
///
/// The zero vector means "holding nothing". A longer vector would be a client
/// asking for more speed than a direction can express, so it is refused at
/// decode rather than clamped: the realm validates movement against the
/// mover's own speed, and a malformed direction is not an input to clamp but
/// a frame to refuse.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct Direction {
    x: i16,
    y: i16,
}

impl Direction {
    /// Build a direction, refusing one longer than a unit vector.
    ///
    /// # Errors
    ///
    /// [`WireError::FieldOutOfRange`] when a component is `i16::MIN` — which
    /// has no positive counterpart, so it could not be negated — or when the
    /// vector is too long.
    pub fn new(x: i16, y: i16) -> Result<Self, WireError> {
        if x == i16::MIN || y == i16::MIN {
            return Err(WireError::FieldOutOfRange);
        }
        let (x64, y64) = (i64::from(x), i64::from(y));
        if x64 * x64 + y64 * y64 > MAX_DIRECTION_MAGNITUDE_SQ {
            return Err(WireError::FieldOutOfRange);
        }
        Ok(Self { x, y })
    }

    /// The still direction.
    #[must_use]
    pub const fn still() -> Self {
        Self { x: 0, y: 0 }
    }

    /// Eastward component, in Q1.15.
    #[must_use]
    pub const fn x(self) -> i16 {
        self.x
    }

    /// Southward component, in Q1.15.
    #[must_use]
    pub const fn y(self) -> i16 {
        self.y
    }
}

/// What an aimed action is aimed at.
///
/// The point is always present — an area spell has no target entity — and the
/// target entity is present only when the client picked one. The view time is
/// what a lag-compensated rewind is computed from.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Aim {
    /// The entity the client believes it is aiming at, when it picked one.
    pub target: Option<EntityId>,
    /// Where in the world the aim lands.
    pub at: WorldPoint,
    /// When the client saw what it aimed at.
    pub viewed: TickInstant,
}

impl Aim {
    /// Read one aim.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] on a short read, or
    /// [`WireError::FieldOutOfRange`] for a presence byte that is not `0`
    /// or `1`.
    pub fn read(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let has_target = r.flag()?;
        let target = r.u64()?;
        let at = WorldPoint {
            x: r.i32()?,
            y: r.i32()?,
        };
        let viewed = TickInstant::read(r)?;
        // The id field is fixed-width whether or not a target is named, so
        // the record stays one size; an absent target must leave it zero or
        // the encoding is not canonical.
        if !has_target && target != 0 {
            return Err(WireError::NonCanonicalPadding);
        }
        Ok(Self {
            target: has_target.then_some(EntityId(target)),
            at,
            viewed,
        })
    }

    /// Write one aim.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when the output is full.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), WireError> {
        w.flag(self.target.is_some())?;
        w.u64(self.target.map_or(0, |t| t.0))?;
        w.i32(self.at.x)?;
        w.i32(self.at.y)?;
        self.viewed.write(w)
    }
}

/// One entity as a client sees it, at a tick.
///
/// Deliberately the minimum the client needs to *draw* the entity and
/// interpolate it: identity, what to draw, where it is, where it is going,
/// and which way it faces. Health, resources, equipment and status are the
/// simulation's and join this record with the item that introduces them —
/// a field with no consumer is surface nobody has reviewed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct EntityState {
    /// The entity's identity within the zone.
    pub id: EntityId,
    /// What it is.
    pub kind: EntityKind,
    /// Where it is.
    pub at: WorldPoint,
    /// How it is moving, per tick, so the client can extrapolate between
    /// authoritative states.
    pub motion: WorldVector,
    /// Which way it faces.
    pub facing: Facing,
}

impl WireItem for EntityState {
    const WIRE_LEN: usize = ENTITY_STATE_LEN;

    fn read(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Ok(Self {
            id: EntityId(r.u64()?),
            kind: EntityKind(r.u16()?),
            at: WorldPoint {
                x: r.i32()?,
                y: r.i32()?,
            },
            motion: WorldVector {
                x: r.i16()?,
                y: r.i16()?,
            },
            facing: Facing(r.u16()?),
        })
    }

    fn write(&self, w: &mut Writer<'_>) -> Result<(), WireError> {
        w.u64(self.id.0)?;
        w.u16(self.kind.0)?;
        w.i32(self.at.x)?;
        w.i32(self.at.y)?;
        w.i16(self.motion.x)?;
        w.i16(self.motion.y)?;
        w.u16(self.facing.0)
    }
}

impl WireItem for EntityId {
    const WIRE_LEN: usize = ENTITY_ID_LEN;

    fn read(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Ok(Self(r.u64()?))
    }

    fn write(&self, w: &mut Writer<'_>) -> Result<(), WireError> {
        w.u64(self.0)
    }
}

/// Whether a resource node can be gathered.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NodeState {
    /// Gatherable now.
    Available,
    /// Gathered out, with no respawn under way.
    Depleted,
    /// Gathered out and respawning.
    Respawning,
}

impl NodeState {
    const fn as_u8(self) -> u8 {
        match self {
            Self::Available => 1,
            Self::Depleted => 2,
            Self::Respawning => 3,
        }
    }

    const fn from_u8(value: u8) -> Result<Self, WireError> {
        Ok(match value {
            1 => Self::Available,
            2 => Self::Depleted,
            3 => Self::Respawning,
            _ => return Err(WireError::UnknownDiscriminant),
        })
    }
}

/// What a stored world edit changed.
///
/// These are the client-visible members of the realm's world-delta schema.
/// Container contents are *not* here and never will be: an unopened
/// container's contents are exactly the kind of secret the realm keeps
/// server-side, and shipping them to a client that has not opened it would
/// hand a cheat the answer.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WorldChange {
    /// The ground was raised or lowered, in height sub-units.
    Height(i16),
    /// A material's weight in the splat field changed, so the terrain blends
    /// differently here.
    Material {
        /// Which material.
        material: u16,
        /// Its new weight, `0` to `255`.
        weight: u8,
    },
    /// A structure was placed, or — when `None` — cleared.
    Structure(Option<StructureId>),
    /// A resource node changed state.
    ResourceNode {
        /// Which node.
        node: ResourceNodeId,
        /// Its new state.
        state: NodeState,
    },
}

/// One stored change to the generated world, at a cell within a chunk.
///
/// The cell offset is bounded by its own type. Whether it lies inside the
/// realm's chunk geometry is the generator's question, answered where the
/// edit is applied — this crate does not carry a second copy of the chunk
/// size to check it against.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct WorldEdit {
    /// Eastward cell offset within the chunk.
    pub cell_x: u16,
    /// Southward cell offset within the chunk.
    pub cell_y: u16,
    /// What changed.
    pub change: WorldChange,
}

impl WireItem for WorldEdit {
    const WIRE_LEN: usize = WORLD_EDIT_LEN;

    fn read(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let cell_x = r.u16()?;
        let cell_y = r.u16()?;
        let kind = r.u8()?;
        let (change, used) = match kind {
            1 => (WorldChange::Height(r.i16()?), 2),
            2 => (
                WorldChange::Material {
                    material: r.u16()?,
                    weight: r.u8()?,
                },
                3,
            ),
            3 => {
                let present = r.flag()?;
                let id = r.u32()?;
                if !present && id != 0 {
                    return Err(WireError::NonCanonicalPadding);
                }
                (
                    WorldChange::Structure(present.then_some(StructureId(id))),
                    5,
                )
            }
            4 => (
                WorldChange::ResourceNode {
                    node: ResourceNodeId(r.u32()?),
                    state: NodeState::from_u8(r.u8()?)?,
                },
                5,
            ),
            _ => return Err(WireError::UnknownDiscriminant),
        };
        r.padding(WORLD_CHANGE_PAYLOAD_LEN - used)?;
        Ok(Self {
            cell_x,
            cell_y,
            change,
        })
    }

    fn write(&self, w: &mut Writer<'_>) -> Result<(), WireError> {
        w.u16(self.cell_x)?;
        w.u16(self.cell_y)?;
        let used = match self.change {
            WorldChange::Height(height) => {
                w.u8(1)?;
                w.i16(height)?;
                2
            }
            WorldChange::Material { material, weight } => {
                w.u8(2)?;
                w.u16(material)?;
                w.u8(weight)?;
                3
            }
            WorldChange::Structure(structure) => {
                w.u8(3)?;
                w.flag(structure.is_some())?;
                w.u32(structure.map_or(0, |s| s.0))?;
                5
            }
            WorldChange::ResourceNode { node, state } => {
                w.u8(4)?;
                w.u32(node.0)?;
                w.u8(state.as_u8())?;
                5
            }
        };
        w.padding(WORLD_CHANGE_PAYLOAD_LEN - used)
    }
}

/// What happened, for the client to play a sound or an effect for.
///
/// The realm decides the outcome; this is the notification. Each carries
/// where it happened, because positional audio and a hit's presentation
/// need a point even when the entity it belonged to has just left.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PlayEvent {
    /// An entity took damage.
    Damage {
        /// Who was hit.
        target: EntityId,
        /// Who hit them, when the realm attributes it.
        source: Option<EntityId>,
        /// How much, for the damage figure.
        amount: u32,
    },
    /// A spell was cast.
    Cast {
        /// Who cast it.
        caster: EntityId,
        /// Which spell.
        spell: SpellId,
    },
    /// An item was picked up.
    Pickup {
        /// Who picked it up.
        actor: EntityId,
        /// Which item.
        item: ItemId,
    },
    /// An entity died.
    Death {
        /// Which entity.
        entity: EntityId,
    },
}

/// One notification, at the tick it happened and the point it happened at.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GameEvent {
    /// The authoritative tick it happened on.
    pub tick: u64,
    /// Where it happened.
    pub at: WorldPoint,
    /// What happened.
    pub event: PlayEvent,
}

impl WireItem for GameEvent {
    const WIRE_LEN: usize = GAME_EVENT_LEN;

    fn read(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let tick = r.u64()?;
        let kind = r.u8()?;
        let at = WorldPoint {
            x: r.i32()?,
            y: r.i32()?,
        };
        let (event, used) = match kind {
            1 => {
                let target = EntityId(r.u64()?);
                let has_source = r.flag()?;
                let source = r.u64()?;
                if !has_source && source != 0 {
                    return Err(WireError::NonCanonicalPadding);
                }
                let amount = r.u32()?;
                (
                    PlayEvent::Damage {
                        target,
                        source: has_source.then_some(EntityId(source)),
                        amount,
                    },
                    21,
                )
            }
            2 => (
                PlayEvent::Cast {
                    caster: EntityId(r.u64()?),
                    spell: SpellId(r.u16()?),
                },
                10,
            ),
            3 => (
                PlayEvent::Pickup {
                    actor: EntityId(r.u64()?),
                    item: ItemId(r.u32()?),
                },
                12,
            ),
            4 => (
                PlayEvent::Death {
                    entity: EntityId(r.u64()?),
                },
                8,
            ),
            _ => return Err(WireError::UnknownDiscriminant),
        };
        r.padding(PLAY_EVENT_PAYLOAD_LEN - used)?;
        Ok(Self { tick, at, event })
    }

    fn write(&self, w: &mut Writer<'_>) -> Result<(), WireError> {
        w.u64(self.tick)?;
        let kind = match self.event {
            PlayEvent::Damage { .. } => 1,
            PlayEvent::Cast { .. } => 2,
            PlayEvent::Pickup { .. } => 3,
            PlayEvent::Death { .. } => 4,
        };
        w.u8(kind)?;
        w.i32(self.at.x)?;
        w.i32(self.at.y)?;
        let used = match self.event {
            PlayEvent::Damage {
                target,
                source,
                amount,
            } => {
                w.u64(target.0)?;
                w.flag(source.is_some())?;
                w.u64(source.map_or(0, |s| s.0))?;
                w.u32(amount)?;
                21
            }
            PlayEvent::Cast { caster, spell } => {
                w.u64(caster.0)?;
                w.u16(spell.0)?;
                10
            }
            PlayEvent::Pickup { actor, item } => {
                w.u64(actor.0)?;
                w.u32(item.0)?;
                12
            }
            PlayEvent::Death { entity } => {
                w.u64(entity.0)?;
                8
            }
        };
        w.padding(PLAY_EVENT_PAYLOAD_LEN - used)
    }
}

#[cfg(test)]
mod tests;
