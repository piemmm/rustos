//! Stateful property model for `WinterSun`'s simulation.
//!
//! The plan's verification list asks for a property test that no sequence of
//! legal actions produces a negative or overflowing stat, resource, or
//! balance. This is it: a randomised program of legal zone operations —
//! spawning, holding a direction, striking, healing, afflicting, spending,
//! stepping — is replayed against a live [`Zone`], and after **every**
//! command the whole zone is checked against the invariants the rules claim
//! to keep.
//!
//! Two things make the check sharp. The workspace builds with overflow
//! checks on in every profile, so any arithmetic that wrapped would abort
//! the case rather than quietly produce a wrong number. And the invariants
//! are read off the live state rather than off a parallel model, because the
//! claim here is about bounds the types must hold at all times, not about
//! agreeing with a second implementation of the same arithmetic.
//!
//! Unlike a fuzz harness, which hammers bytes looking for crashes, this
//! generates *structured* programs and lets proptest shrink a
//! counterexample to a minimal failing one.

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use tairix_wintersun_net::client::{Intent, IntentKind};
use tairix_wintersun_net::value::{
    Direction, EntityId, EntityKind, TickInstant, TickPhase, WorldPoint,
};
use tairix_wintersun_rules::bounds::{
    MAX_ARMOUR, MAX_BLOW_BASE, MAX_BODY_RADIUS_SUB_UNITS, MAX_HARMFUL_STATUS, MAX_HELPFUL_STATUS,
    MAX_POWER_SCALE_PERMILLE, MAX_SPEED_SUB_UNITS_PER_TICK, MAX_STAT, MAX_STATUS_TICKS,
};
use tairix_wintersun_rules::clock::TickRate;
use tairix_wintersun_rules::damage::{Blow, School};
use tairix_wintersun_rules::entity::SpawnSpec;
use tairix_wintersun_rules::motion::effective_speed;
use tairix_wintersun_rules::stat::{Stat, Stats};
use tairix_wintersun_rules::status::{Status, StatusKind};
use tairix_wintersun_rules::terrain::SyntheticTerrain;
use tairix_wintersun_rules::zone::Zone;

/// Sequences run once by a plain `cargo test` (no budget set).
const SMOKE_CASES: u32 = 96;

/// Sequences per batch under a wall-clock budget.
const BUDGET_BATCH_CASES: u32 = 192;

/// Commands in one generated program.
const PROGRAM_LEN: usize = 48;

/// Bodies a program may hold, so a long spawn run does not turn the model
/// into a throughput test.
const MAX_BODIES: usize = 8;

/// One legal thing a caller may do to a zone.
#[derive(Copy, Clone, Debug)]
enum Cmd {
    Spawn {
        cell_x: i32,
        cell_y: i32,
        stats: [u16; 5],
        armour: u16,
        radius: u16,
    },
    Hold {
        body: usize,
        x: i16,
        y: i16,
    },
    Strike {
        attacker: usize,
        target: usize,
        school: usize,
        base: u32,
        scale: u16,
    },
    Heal {
        healer: usize,
        target: usize,
        base: u32,
    },
    Afflict {
        source: usize,
        target: usize,
        kind: usize,
        magnitude: u16,
        ticks: u32,
    },
    Spend {
        body: usize,
        amount: u32,
    },
    Despawn {
        body: usize,
    },
    Step,
}

fn command() -> impl Strategy<Value = Cmd> {
    prop_oneof![
        4 => (-40_i32..40, -40_i32..40, prop::array::uniform5(0u16..=MAX_STAT), 0u16..=MAX_ARMOUR, 1u16..=2048)
            .prop_map(|(cell_x, cell_y, stats, armour, radius)| Cmd::Spawn {
                cell_x,
                cell_y,
                stats,
                armour,
                radius,
            }),
        8 => (any::<usize>(), -32_767i16..=32_767, -32_767i16..=32_767)
            .prop_map(|(body, x, y)| Cmd::Hold { body, x, y }),
        6 => (any::<usize>(), any::<usize>(), any::<usize>(), 0u32..=MAX_BLOW_BASE, 0u16..=MAX_POWER_SCALE_PERMILLE)
            .prop_map(|(attacker, target, school, base, scale)| Cmd::Strike {
                attacker,
                target,
                school,
                base,
                scale,
            }),
        4 => (any::<usize>(), any::<usize>(), 0u32..100_000)
            .prop_map(|(healer, target, base)| Cmd::Heal {
                healer,
                target,
                base,
            }),
        6 => (any::<usize>(), any::<usize>(), any::<usize>(), any::<u16>(), 1u32..=MAX_STATUS_TICKS)
            .prop_map(|(source, target, kind, magnitude, ticks)| Cmd::Afflict {
                source,
                target,
                kind,
                magnitude,
                ticks,
            }),
        3 => (any::<usize>(), any::<u32>()).prop_map(|(body, amount)| Cmd::Spend { body, amount }),
        1 => any::<usize>().prop_map(|body| Cmd::Despawn { body }),
        8 => Just(Cmd::Step),
    ]
}

fn program() -> impl Strategy<Value = Vec<Cmd>> {
    prop::collection::vec(command(), 1..=PROGRAM_LEN)
}

/// Every invariant the rules claim, read off the live zone.
fn check(zone: &Zone) -> Result<(), TestCaseError> {
    for body in zone.entities() {
        let health = body.health();
        let resource = body.resource();
        prop_assert!(
            health.current() <= health.max(),
            "health {} exceeded its maximum {}",
            health.current(),
            health.max()
        );
        prop_assert!(
            resource.current() <= resource.max(),
            "resource {} exceeded its maximum {}",
            resource.current(),
            resource.max()
        );
        prop_assert_eq!(health.max(), body.stats().max_health());
        prop_assert_eq!(resource.max(), body.stats().max_resource());

        for &stat in Stat::ALL {
            prop_assert!(body.stats().get(stat) <= MAX_STAT, "a stat left its domain");
        }
        prop_assert!(
            body.stats().resistance_permille() < 1000,
            "resistance reached total, which would negate a blow entirely"
        );

        let speed = effective_speed(body.stats(), body.status());
        prop_assert!(
            (0..=MAX_SPEED_SUB_UNITS_PER_TICK).contains(&speed),
            "speed {speed} left its bound"
        );

        let (residue_x, residue_y) = body.residue();
        prop_assert!(
            (0..(1 << 15)).contains(&residue_x) && (0..(1 << 15)).contains(&residue_y),
            "a carried remainder left its scale"
        );

        prop_assert!(
            i32::from(body.radius()) <= MAX_BODY_RADIUS_SUB_UNITS && body.radius() > 0,
            "a body radius left its bound"
        );

        let harmful = body
            .status()
            .held()
            .iter()
            .filter(|s| s.kind().is_harmful())
            .count();
        let helpful = body.status().held().len() - harmful;
        prop_assert!(
            harmful <= MAX_HARMFUL_STATUS,
            "the harmful partition overflowed"
        );
        prop_assert!(
            helpful <= MAX_HELPFUL_STATUS,
            "the helpful partition overflowed"
        );
        for status in body.status().held() {
            prop_assert!(status.remaining() > 0, "an expired status was still held");
            prop_assert!(status.remaining() <= MAX_STATUS_TICKS);
            prop_assert!(status.magnitude() <= status.kind().magnitude_ceiling());
        }

        prop_assert!(
            body.status().damage_taken_permille() > 0,
            "mitigation reached total, which would make a body invulnerable"
        );
        prop_assert!(body.status().healing_permille() <= 1000);
    }

    // The identity order the whole step depends on.
    prop_assert!(
        zone.entities().windows(2).all(|pair| match pair {
            [a, b] => a.id() < b.id(),
            _ => true,
        }),
        "the entity table lost its identity order"
    );
    Ok(())
}

/// The zone a program is replayed against, and the model's own view of who
/// is live.
struct Session {
    zone: Zone,
    ids: Vec<EntityId>,
    sequence: Vec<u64>,
    ground: SyntheticTerrain,
}

impl Session {
    fn new() -> Self {
        Self {
            zone: Zone::new(TickRate::default_rate()),
            ids: Vec::new(),
            sequence: Vec::new(),
            ground: SyntheticTerrain::lattice(7, 5),
        }
    }

    /// Pick a live body, or `None` when the zone is empty.
    fn pick(&self, index: usize) -> Option<EntityId> {
        if self.ids.is_empty() {
            return None;
        }
        self.ids.get(index % self.ids.len()).copied()
    }

    fn spawn(&mut self, cell_x: i32, cell_y: i32, stats: [u16; 5], armour: u16, radius: u16) {
        if self.ids.len() >= MAX_BODIES {
            return;
        }
        let stats = Stats::new(stats[0], stats[1], stats[2], stats[3], stats[4])
            .expect("drawn inside the domain");
        let at = WorldPoint {
            x: cell_x * 1024 + 512,
            y: cell_y * 1024 + 512,
        };
        let spec = SpawnSpec::new(EntityKind(1), at, stats, armour, radius)
            .expect("drawn inside the bounds");
        self.ids
            .push(self.zone.spawn(spec).expect("room for a body"));
        self.sequence.push(0);
    }

    fn hold(&mut self, body: usize, x: i16, y: i16) {
        let Some(id) = self.pick(body) else { return };
        let slot = body % self.ids.len();
        // A direction longer than a unit is refused at decode, so the model
        // only ever offers a legal one: halving the pair keeps the draw wide
        // without making an illegal action out of it.
        let direction = Direction::new(x / 2, y / 2).unwrap_or_else(|_| Direction::still());
        let Some(counter) = self.sequence.get_mut(slot) else {
            return;
        };
        *counter += 1;
        let intent = Intent {
            sequence: *counter,
            sampled: TickInstant {
                tick: self.zone.tick(),
                phase: TickPhase(0),
            },
            kind: IntentKind::Move(direction),
        };
        // A refusal is a legitimate answer, not a failure: the per-tick
        // budget is finite by design.
        let _ = self.zone.submit(id, &intent);
    }

    fn step(&mut self) {
        self.zone
            .step(&self.ground)
            .expect("the step does not run out of memory");
        // The reap removes the dead, so the model's own view of who is live
        // has to follow it.
        self.ids.retain(|id| self.zone.entity(*id).is_some());
        self.sequence.truncate(self.ids.len());
    }

    fn run(&mut self, command: Cmd) {
        match command {
            Cmd::Spawn {
                cell_x,
                cell_y,
                stats,
                armour,
                radius,
            } => self.spawn(cell_x, cell_y, stats, armour, radius),
            Cmd::Hold { body, x, y } => self.hold(body, x, y),
            Cmd::Strike {
                attacker,
                target,
                school,
                base,
                scale,
            } => {
                if let (Some(attacker), Some(target)) = (self.pick(attacker), self.pick(target)) {
                    let school = School::ALL
                        .get(school % School::ALL.len())
                        .copied()
                        .unwrap_or(School::Physical);
                    let blow = Blow::new(school, base, scale).expect("drawn inside the bounds");
                    let _ = self.zone.apply_blow(Some(attacker), target, &blow);
                }
            }
            Cmd::Heal {
                healer,
                target,
                base,
            } => {
                if let (Some(healer), Some(target)) = (self.pick(healer), self.pick(target)) {
                    let _ = self.zone.apply_heal(Some(healer), target, base);
                }
            }
            Cmd::Afflict {
                source,
                target,
                kind,
                magnitude,
                ticks,
            } => {
                if let (Some(source), Some(target)) = (self.pick(source), self.pick(target)) {
                    let kind = StatusKind::ALL
                        .get(kind % StatusKind::COUNT)
                        .copied()
                        .unwrap_or(StatusKind::Bleed);
                    // Clamped to the kind's own ceiling, so the model applies
                    // legal statuses rather than exercising the constructor's
                    // refusal, which the unit tests own.
                    let magnitude = magnitude.min(kind.magnitude_ceiling());
                    let status = Status::new(kind, magnitude, ticks, source)
                        .expect("drawn inside the bounds");
                    let _ = self.zone.apply_status(target, status);
                }
            }
            Cmd::Spend { body, amount } => {
                if let Some(id) = self.pick(body) {
                    let _ = self.zone.spend_resource(id, amount);
                }
            }
            Cmd::Despawn { body } => {
                if let Some(id) = self.pick(body) {
                    self.zone.despawn(id);
                    self.ids.retain(|held| *held != id);
                    self.sequence.truncate(self.ids.len());
                }
            }
            Cmd::Step => self.step(),
        }
    }
}

#[test]
fn no_sequence_of_legal_actions_breaks_a_bound() {
    tairix_fuzzseed::prop::drive(
        "no_sequence_of_legal_actions_breaks_a_bound",
        SMOKE_CASES,
        BUDGET_BATCH_CASES,
        program(),
        |commands| {
            let mut session = Session::new();
            for command in commands {
                session.run(command);
                check(&session.zone)?;
            }
            Ok(())
        },
    );
}
