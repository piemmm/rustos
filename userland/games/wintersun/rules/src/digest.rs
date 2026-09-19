//! The state digest, and the scripted run the determinism claim is staked
//! on.
//!
//! # Two uses, one fold
//!
//! A deterministic simulation has a state hash, and that hash does two jobs.
//! It is how a desync is *bisected*: a client and a realm exchange hashes
//! periodically, and on a mismatch the answer is "entity 412 diverged at
//! tick 90 113" rather than "the players disagree" — which is the difference
//! between minutes and days. And it is how the cross-target claim is
//! *stated*: every Tier-1 target runs one scripted session and asserts one
//! constant, [`REFERENCE_DIGEST`].
//!
//! Comparing two runs would prove nothing on its own, because both could be
//! wrong together. Agreement between targets follows from each agreeing with
//! the constant, so a target that has never been run cannot pass by
//! accident.
//!
//! # What the scripted run is, and what it deliberately is not
//!
//! It is a session: twelve bodies of differing stats walking a changing set
//! of directions over ground with obstacles in it, taking blows, being
//! healed, carrying statuses that stack and diminish, colliding with each
//! other, and one of them dying. The digest folds the *whole trajectory*,
//! every tick — not the final state — so a divergence that later converges
//! is still caught.
//!
//! Its ground is a pattern rather than a generated realm, and that is the
//! point: the world generator has its own determinism vertical and its own
//! constant, and conflating the two would make each fragile to the other's
//! changes while proving nothing extra. The claim here is about the
//! *simulation*.

use core::hash::Hasher;

use tairix_hash::FastHash;
use tairix_wintersun_net::client::{Intent, IntentKind};
use tairix_wintersun_net::value::{
    Direction, EntityId, EntityKind, TickInstant, TickPhase, WorldPoint,
};

use crate::clock::TickRate;
use crate::damage::{Blow, School};
use crate::entity::{Entity, SpawnSpec};
use crate::error::{Refusal, RulesError};
use crate::stat::{Stat, Stats};
use crate::status::{Status, StatusKind};
use crate::terrain::SyntheticTerrain;
use crate::zone::Zone;

/// The digest of the scripted run, on every Tier-1 target.
///
/// Changing any rule changes this. That is the point: the constant is not a
/// magic number to be re-derived when a test fails, it is the record of what
/// the simulation does. A change that moves it is a change to every fight
/// anybody has ever had, and the new value is written down deliberately
/// rather than pasted out of a failure.
pub const REFERENCE_DIGEST: u64 = 0x15C6_4F9A_10E8_91BD;

/// Bodies the scripted run spawns.
pub const REFERENCE_BODIES: usize = 12;

/// Ticks it runs — eight seconds at the default rate, which is long enough
/// for a status to expire, a diminishing chain to run out, and a body to
/// cross several cells.
pub const REFERENCE_TICKS: u64 = 240;

/// Cell spacing of the reference ground's pillars.
const PILLAR_PERIOD: i32 = 9;

/// Cell spacing of the reference ground's pools.
const POOL_PERIOD: i32 = 7;

/// Where the twelve start, in whole world cells from the origin.
///
/// A ring, so they walk into each other rather than apart, and chosen so no
/// body begins standing in a pillar or a pool — a spawn inside an obstacle
/// would make the run's first tick about the spawn rather than about the
/// rules.
const RING: [(i32, i32); REFERENCE_BODIES] = [
    (0, -6),
    (3, -5),
    (5, -3),
    (6, 0),
    (5, 3),
    (3, 5),
    (0, 6),
    (-3, 5),
    (-5, 3),
    (-6, 0),
    (-5, -3),
    (-3, -5),
];

/// The eight directions the run walks, as the wire's own Q1.15.
///
/// The diagonals are the unit vector's components rather than a full unit on
/// each axis, so a diagonal walk is not faster than a cardinal one.
const HEADINGS: [(i16, i16); 8] = [
    (32_767, 0),
    (23_170, 23_170),
    (0, 32_767),
    (-23_170, 23_170),
    (-32_767, 0),
    (-23_170, -23_170),
    (0, -32_767),
    (23_170, -23_170),
];

/// One scripted thing that happens to the run.
#[derive(Copy, Clone, Debug)]
enum Act {
    /// A body strikes another.
    Strike {
        attacker: usize,
        target: usize,
        school: School,
        base: u32,
        scale: u16,
    },
    /// A body heals another.
    Mend {
        healer: usize,
        target: usize,
        base: u32,
    },
    /// A body puts a status on another.
    Afflict {
        source: usize,
        target: usize,
        kind: StatusKind,
        magnitude: u16,
        ticks: u32,
    },
}

/// What happens, and when.
///
/// Every rule the item owns is reached at least once: each school of damage,
/// both mitigations, a shield that absorbs, a heal under a withering effect,
/// a periodic effect on both signs, a control effect applied often enough to
/// run out its diminishing chain, and a blow large enough to kill.
const SCRIPT: &[(u64, Act)] = &[
    (
        10,
        Act::Afflict {
            source: 1,
            target: 0,
            kind: StatusKind::Slow,
            magnitude: 400,
            ticks: 60,
        },
    ),
    (
        12,
        Act::Afflict {
            source: 2,
            target: 1,
            kind: StatusKind::Haste,
            magnitude: 300,
            ticks: 90,
        },
    ),
    (
        15,
        Act::Afflict {
            source: 3,
            target: 2,
            kind: StatusKind::Bleed,
            magnitude: 7,
            ticks: 45,
        },
    ),
    (
        18,
        Act::Afflict {
            source: 3,
            target: 3,
            kind: StatusKind::Regenerate,
            magnitude: 5,
            ticks: 45,
        },
    ),
    (
        20,
        Act::Afflict {
            source: 4,
            target: 4,
            kind: StatusKind::Shield,
            magnitude: 250,
            ticks: 120,
        },
    ),
    (
        22,
        Act::Afflict {
            source: 5,
            target: 5,
            kind: StatusKind::Fortify,
            magnitude: 350,
            ticks: 120,
        },
    ),
    (
        24,
        Act::Afflict {
            source: 6,
            target: 6,
            kind: StatusKind::Vulnerable,
            magnitude: 500,
            ticks: 120,
        },
    ),
    (
        26,
        Act::Afflict {
            source: 7,
            target: 7,
            kind: StatusKind::Wither,
            magnitude: 600,
            ticks: 120,
        },
    ),
    (
        28,
        Act::Afflict {
            source: 8,
            target: 9,
            kind: StatusKind::Root,
            magnitude: 0,
            ticks: 20,
        },
    ),
    (
        30,
        Act::Afflict {
            source: 8,
            target: 10,
            kind: StatusKind::Silence,
            magnitude: 0,
            ticks: 30,
        },
    ),
    (
        32,
        Act::Strike {
            attacker: 0,
            target: 4,
            school: School::Physical,
            base: 300,
            scale: 1000,
        },
    ),
    (
        34,
        Act::Strike {
            attacker: 1,
            target: 5,
            school: School::Frost,
            base: 300,
            scale: 1000,
        },
    ),
    (
        36,
        Act::Strike {
            attacker: 2,
            target: 6,
            school: School::Flame,
            base: 300,
            scale: 800,
        },
    ),
    (
        38,
        Act::Strike {
            attacker: 3,
            target: 7,
            school: School::Storm,
            base: 200,
            scale: 1200,
        },
    ),
    (
        40,
        Act::Strike {
            attacker: 4,
            target: 8,
            school: School::Blight,
            base: 150,
            scale: 0,
        },
    ),
    (
        42,
        Act::Strike {
            attacker: 5,
            target: 9,
            school: School::Radiant,
            base: 150,
            scale: 500,
        },
    ),
    (
        44,
        Act::Mend {
            healer: 6,
            target: 7,
            base: 120,
        },
    ),
    (
        46,
        Act::Mend {
            healer: 6,
            target: 4,
            base: 120,
        },
    ),
    // One control effect applied four times inside its window: full, half,
    // quarter, then refused.
    (
        60,
        Act::Afflict {
            source: 11,
            target: 8,
            kind: StatusKind::Stun,
            magnitude: 0,
            ticks: 32,
        },
    ),
    (
        95,
        Act::Afflict {
            source: 11,
            target: 8,
            kind: StatusKind::Stun,
            magnitude: 0,
            ticks: 32,
        },
    ),
    (
        130,
        Act::Afflict {
            source: 11,
            target: 8,
            kind: StatusKind::Stun,
            magnitude: 0,
            ticks: 32,
        },
    ),
    (
        160,
        Act::Afflict {
            source: 11,
            target: 8,
            kind: StatusKind::Stun,
            magnitude: 0,
            ticks: 32,
        },
    ),
    // Enough to kill, so the reap and its notification are on the path.
    (
        180,
        Act::Strike {
            attacker: 0,
            target: 11,
            school: School::Physical,
            base: 900_000,
            scale: 1000,
        },
    ),
];

/// The ground the scripted run walks.
#[must_use]
pub fn reference_terrain() -> SyntheticTerrain {
    SyntheticTerrain::lattice(PILLAR_PERIOD, POOL_PERIOD)
}

/// The body the scripted run spawns at `index`.
///
/// Stats, armour and radius all vary with the index, so no two bodies share
/// a curve evaluation and a stat that had stopped being read would show up
/// as a digest that no longer moved.
///
/// # Errors
///
/// Whatever the spec refuses — never, for these values, and a test holds
/// them inside the bounds.
pub fn reference_body(index: usize) -> Result<SpawnSpec, crate::error::RuleError> {
    let step = u16::try_from(index).unwrap_or(0);
    let stats = Stats::new(
        40 + step * 37,
        30 + step * 23,
        60 + step * 41,
        25 + step * 29,
        20 + step * 31,
    )?;
    let (cell_x, cell_y) = RING.get(index).copied().unwrap_or((0, 0));
    // Cell centres, so a body's footprint starts inside one cell rather than
    // straddling a boundary and making the first tick about the spawn.
    let at = WorldPoint {
        x: cell_x * CELL + CELL / 2,
        y: cell_y * CELL + CELL / 2,
    };
    SpawnSpec::new(
        EntityKind(1 + u16::try_from(index % 3).unwrap_or(0)),
        at,
        stats,
        step * 60,
        256 + step * 16,
    )
}

/// Sub-units to a world cell, which is where a body is placed.
const CELL: i32 = tairix_wintersun_world::geom::CELL_SUB_UNITS;

/// Run the scripted session and fold the whole trajectory into one number.
///
/// # Errors
///
/// Whatever the zone refuses — in practice [`RulesError::OutOfMemory`],
/// since every scripted value is validated before the run starts.
pub fn reference_run() -> Result<u64, RulesError> {
    let mut hasher = FastHash::new();
    scripted(&mut hasher)?;
    Ok(hasher.finish())
}

/// Play the scripted session, folding every tick, and hand back the zone it
/// left — which is what lets a test assert the script did what it says
/// rather than only that its hash is stable.
fn scripted(hasher: &mut FastHash) -> Result<Zone, RulesError> {
    let terrain = reference_terrain();
    let mut zone = Zone::new(TickRate::default_rate());
    let mut ids = [EntityId(0); REFERENCE_BODIES];
    for (index, slot) in ids.iter_mut().enumerate() {
        // A refused spec would be a defect in the constants above, not a
        // runtime condition; the run reports it by producing a digest that
        // does not match rather than by aborting.
        let Ok(spec) = reference_body(index) else {
            continue;
        };
        *slot = zone.spawn(spec)?;
    }

    for tick in 0..REFERENCE_TICKS {
        for (index, &id) in ids.iter().enumerate() {
            let ordinal = u64::try_from(index).unwrap_or(0);
            let heading = HEADINGS
                .get(usize::try_from((tick / 7).wrapping_add(ordinal) % 8).unwrap_or(0))
                .copied()
                .unwrap_or((0, 0));
            let kind = Direction::new(heading.0, heading.1)
                .map_or_else(|_| IntentKind::Move(Direction::still()), IntentKind::Move);
            let phase = u16::try_from((tick * 7 + ordinal * 3) % 16).unwrap_or(0);
            let intent = Intent {
                sequence: tick + 1,
                sampled: TickInstant {
                    tick,
                    phase: TickPhase(phase * 4096),
                },
                kind,
            };
            match zone.submit(id, &intent) {
                Ok(placed) => {
                    hasher.write(&placed.tick.to_le_bytes());
                    hasher.write(&placed.phase.0.to_le_bytes());
                }
                Err(refusal) => hasher.write(&[0xFF, refusal_code(refusal)]),
            }
        }

        for &(at, act) in SCRIPT {
            if at != tick {
                continue;
            }
            perform(&mut zone, &ids, act);
        }

        zone.step(&terrain)?;
        fold_tick(hasher, &zone);
    }
    Ok(zone)
}

/// Apply one scripted act, folding nothing: its effect shows up in the
/// state.
fn perform(zone: &mut Zone, ids: &[EntityId; REFERENCE_BODIES], act: Act) {
    let at = |index: usize| ids.get(index).copied().unwrap_or(EntityId(0));
    match act {
        Act::Strike {
            attacker,
            target,
            school,
            base,
            scale,
        } => {
            if let Ok(blow) = Blow::new(school, base, scale) {
                let _ = zone.apply_blow(Some(at(attacker)), at(target), &blow);
            }
        }
        Act::Mend {
            healer,
            target,
            base,
        } => {
            let _ = zone.apply_heal(Some(at(healer)), at(target), base);
        }
        Act::Afflict {
            source,
            target,
            kind,
            magnitude,
            ticks,
        } => {
            if let Ok(status) = Status::new(kind, magnitude, ticks, at(source)) {
                let _ = zone.apply_status(at(target), status);
            }
        }
    }
}

/// Fold one tick's whole observable state.
fn fold_tick(hasher: &mut FastHash, zone: &Zone) {
    fold_header(hasher, zone);
    for body in zone.entities() {
        fold_entity(hasher, body);
    }
    for event in zone.events() {
        hasher.write(&event.tick.to_le_bytes());
        hasher.write(&event.at.x.to_le_bytes());
        hasher.write(&event.at.y.to_le_bytes());
        fold_play_event(hasher, event.event);
    }
    for refused in zone.refusals() {
        hasher.write(&refused.entity.0.to_le_bytes());
        hasher.write(&refused.sequence.to_le_bytes());
        hasher.write(&[refusal_code(refused.reason)]);
    }
}

/// One body's hash, which is what a desync is bisected down to.
#[must_use]
pub fn entity(body: &Entity) -> u64 {
    let mut hasher = FastHash::new();
    fold_entity(&mut hasher, body);
    hasher.finish()
}

/// One zone's hash, which is what two ends exchange.
#[must_use]
pub fn zone(zone: &Zone) -> u64 {
    let mut hasher = FastHash::new();
    fold_header(&mut hasher, zone);
    for body in zone.entities() {
        fold_entity(&mut hasher, body);
    }
    hasher.finish()
}

/// The tick and the population, at a width that does not follow the target's
/// pointer size.
fn fold_header(hasher: &mut FastHash, zone: &Zone) {
    hasher.write(&zone.tick().to_le_bytes());
    let population = u64::try_from(zone.population()).unwrap_or(u64::MAX);
    hasher.write(&population.to_le_bytes());
}

/// Every stored field of one body, in a stated order.
fn fold_entity(hasher: &mut FastHash, body: &Entity) {
    hasher.write(&body.id().0.to_le_bytes());
    hasher.write(&body.kind().0.to_le_bytes());
    hasher.write(&body.at().x.to_le_bytes());
    hasher.write(&body.at().y.to_le_bytes());
    hasher.write(&body.facing().0.to_le_bytes());
    hasher.write(&body.radius().to_le_bytes());
    hasher.write(&body.armour().to_le_bytes());
    for &stat in Stat::ALL {
        hasher.write(&body.stats().get(stat).to_le_bytes());
    }
    hasher.write(&body.health().current().to_le_bytes());
    hasher.write(&body.health().max().to_le_bytes());
    hasher.write(&body.resource().current().to_le_bytes());
    hasher.write(&body.resource().max().to_le_bytes());
    hasher.write(&body.held().x().to_le_bytes());
    hasher.write(&body.held().y().to_le_bytes());
    hasher.write(&body.motion().x.to_le_bytes());
    hasher.write(&body.motion().y.to_le_bytes());
    let (residue_x, residue_y) = body.residue();
    hasher.write(&residue_x.to_le_bytes());
    hasher.write(&residue_y.to_le_bytes());
    hasher.write(&body.acknowledged().to_le_bytes());
    for status in body.status().held() {
        hasher.write(&[status_code(status.kind())]);
        hasher.write(&status.magnitude().to_le_bytes());
        hasher.write(&status.remaining().to_le_bytes());
        hasher.write(&status.source().0.to_le_bytes());
    }
}

/// A stable byte per status kind, so the digest does not move with a
/// variant's declaration order.
fn status_code(kind: StatusKind) -> u8 {
    u8::try_from(kind.index()).unwrap_or(u8::MAX)
}

/// A stable byte per refusal.
fn refusal_code(refusal: Refusal) -> u8 {
    match refusal {
        Refusal::NoSuchEntity(_) => 1,
        Refusal::Replayed => 2,
        Refusal::SampledInTheFuture => 3,
        Refusal::TooManyThisTick => 4,
        Refusal::Stunned => 5,
        Refusal::Silenced => 6,
        Refusal::Unresolvable => 7,
    }
}

/// One notification, discriminant and payload both: an amount that stopped
/// being computed would otherwise leave the digest unmoved.
fn fold_play_event(hasher: &mut FastHash, event: tairix_wintersun_net::value::PlayEvent) {
    use tairix_wintersun_net::value::PlayEvent;
    match event {
        PlayEvent::Damage {
            target,
            source,
            amount,
        } => {
            hasher.write(&[1]);
            hasher.write(&target.0.to_le_bytes());
            hasher.write(&source.map_or(0, |id| id.0).to_le_bytes());
            hasher.write(&amount.to_le_bytes());
        }
        PlayEvent::Cast { caster, spell } => {
            hasher.write(&[2]);
            hasher.write(&caster.0.to_le_bytes());
            hasher.write(&spell.0.to_le_bytes());
        }
        PlayEvent::Pickup { actor, item } => {
            hasher.write(&[3]);
            hasher.write(&actor.0.to_le_bytes());
            hasher.write(&item.0.to_le_bytes());
        }
        PlayEvent::Death { entity } => {
            hasher.write(&[4]);
            hasher.write(&entity.0.to_le_bytes());
        }
    }
}

#[cfg(test)]
mod tests;
