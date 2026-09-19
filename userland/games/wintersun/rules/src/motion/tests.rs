use tairix_wintersun_net::value::{Direction, EntityId, EntityKind, WorldPoint};
use tairix_wintersun_world::geom::{CellCoord, Elevation, CELL_SUB_UNITS};

use super::{effective_speed, footprint_clear, separation, step};
use crate::entity::{Entity, SpawnSpec, RESIDUE_SCALE};
use crate::stat::Stats;
use crate::status::{Status, StatusKind};
use crate::terrain::{cell_at, SyntheticTerrain, Terrain, TerrainCell};

/// Ground the tests place obstacles on by hand.
struct Patch {
    blocked: &'static [(i32, i32)],
}

impl Terrain for Patch {
    fn cell(&self, cell: CellCoord) -> Option<TerrainCell> {
        if self.blocked.contains(&(cell.x, cell.y)) {
            return Some(TerrainCell {
                ground: Elevation(0),
                water: Elevation(1000),
            });
        }
        Some(TerrainCell::dry(Elevation(0)))
    }
}

/// Ground the caller has none of.
struct Absent;

impl Terrain for Absent {
    fn cell(&self, _: CellCoord) -> Option<TerrainCell> {
        None
    }
}

fn body(agility: u16, at: WorldPoint, radius: u16) -> Entity {
    let stats = Stats::new(0, agility, 0, 0, 0).expect("inside the domain");
    let spec = SpawnSpec::new(EntityKind(1), at, stats, 0, radius).expect("a legal body");
    Entity::spawn(EntityId(1), spec)
}

fn centre(cell_x: i32, cell_y: i32) -> WorldPoint {
    WorldPoint {
        x: cell_x * CELL_SUB_UNITS + CELL_SUB_UNITS / 2,
        y: cell_y * CELL_SUB_UNITS + CELL_SUB_UNITS / 2,
    }
}

#[test]
fn statuses_scale_the_speed_and_control_effects_stop_it() {
    let stats = Stats::new(0, 500, 0, 0, 0).expect("inside the domain");
    let base = stats.speed_sub_units_per_tick();
    let mut slowed = crate::status::StatusSet::new();
    slowed.apply(Status::new(StatusKind::Slow, 500, 10, EntityId(2)).expect("a legal slow"));
    assert_eq!(effective_speed(stats, &slowed), base / 2);

    let mut hastened = crate::status::StatusSet::new();
    hastened.apply(Status::new(StatusKind::Haste, 500, 10, EntityId(2)).expect("a legal haste"));
    assert_eq!(effective_speed(stats, &hastened), base * 3 / 2);

    for kind in [StatusKind::Root, StatusKind::Stun] {
        let mut held = crate::status::StatusSet::new();
        held.apply(Status::new(kind, 0, 10, EntityId(2)).expect("a legal status"));
        assert_eq!(effective_speed(stats, &held), 0, "{kind:?}");
    }
}

#[test]
fn a_body_holding_nothing_does_not_move() {
    let body = body(500, centre(0, 0), 256);
    let outcome = step(&body, &SyntheticTerrain::open());
    assert_eq!(outcome.at, body.at());
    assert_eq!(outcome.moved.x, 0);
    assert_eq!(outcome.residue, (0, 0));
}

#[test]
fn a_full_unit_step_is_the_speed_itself() {
    let mut body = body(0, centre(0, 0), 256);
    let speed = body.stats().speed_sub_units_per_tick();
    // A full-scale component is one short of the scale, so the first step
    // is one sub-unit shy and the remainder carries the rest.
    body.hold(Direction::new(i16::MAX, 0).expect("east"));
    let outcome = step(&body, &SyntheticTerrain::open());
    assert_eq!(i64::from(outcome.moved.x), i64::from(speed) - 1);
    assert_eq!(outcome.residue.0, RESIDUE_SCALE - i64::from(speed));
}

#[test]
fn the_carried_remainder_makes_a_slow_walk_accumulate_exactly() {
    // A direction of one thirty-second of a unit at any speed would
    // truncate to nothing every tick without the remainder.
    let mut body = body(0, centre(4, 4), 256);
    body.hold(Direction::new(1_024, 0).expect("a thirty-second east"));
    let speed = i64::from(body.stats().speed_sub_units_per_tick());
    let ground = SyntheticTerrain::open();

    let start = body.at().x;
    for _ in 0..32 {
        let outcome = step(&body, &ground);
        body.place(outcome.at, outcome.residue, outcome.moved);
    }
    let travelled = i64::from(body.at().x - start);
    let exact = 32 * 1_024 * speed / RESIDUE_SCALE;
    assert_eq!(
        travelled, exact,
        "thirty-two steps must cover what one exact multiplication says"
    );
    assert!(travelled > 0, "and it must actually move");
}

#[test]
fn a_diagonal_is_not_faster_than_a_cardinal() {
    let ground = SyntheticTerrain::open();
    let mut east = body(300, centre(4, 4), 256);
    east.hold(Direction::new(i16::MAX, 0).expect("east"));
    let mut diagonal = body(300, centre(40, 40), 256);
    diagonal.hold(Direction::new(23_170, 23_170).expect("south-east"));

    for _ in 0..40 {
        let a = step(&east, &ground);
        east.place(a.at, a.residue, a.moved);
        let b = step(&diagonal, &ground);
        diagonal.place(b.at, b.residue, b.moved);
    }
    let straight = i64::from(east.at().x - centre(4, 4).x);
    let dx = i64::from(diagonal.at().x - centre(40, 40).x);
    let dy = i64::from(diagonal.at().y - centre(40, 40).y);
    let along = (dx * dx + dy * dy).isqrt();
    assert!(
        along <= straight,
        "a diagonal covered {along} against a cardinal's {straight}"
    );
}

#[test]
fn a_blocked_diagonal_slides_along_the_wall() {
    // A wall to the south, with the body's southern edge one sub-unit shy
    // of it, so any southward step at all meets it.
    let ground = Patch {
        blocked: &[(3, 5), (4, 5), (5, 5)],
    };
    let against_the_wall = WorldPoint {
        x: centre(4, 4).x,
        y: 5 * CELL_SUB_UNITS - 256 - 1,
    };
    let mut body = body(1000, against_the_wall, 256);
    body.hold(Direction::new(23_170, 23_170).expect("south-east"));
    let outcome = step(&body, &ground);
    assert!(outcome.moved.x > 0, "it kept the part that was clear");
    assert_eq!(outcome.moved.y, 0, "and dropped the part that was not");
}

#[test]
fn a_body_boxed_in_stays_put_but_keeps_its_remainder() {
    let ground = Patch {
        blocked: &[
            (3, 4),
            (5, 4),
            (4, 3),
            (4, 5),
            (3, 3),
            (5, 5),
            (3, 5),
            (5, 3),
        ],
    };
    // Nearly as wide as its cell, so any step at all reaches a neighbour.
    let mut body = body(1000, centre(4, 4), 500);
    body.hold(Direction::new(23_170, 23_170).expect("south-east"));
    let outcome = step(&body, &ground);
    assert_eq!(outcome.at, body.at());
    assert_eq!((outcome.moved.x, outcome.moved.y), (0, 0));
    assert!(
        outcome.residue.0 > 0,
        "the fraction was not refused, it simply has not accumulated"
    );
}

#[test]
fn a_body_cannot_walk_off_the_edge_of_generated_ground() {
    let mut body = body(1000, centre(0, 0), 256);
    body.hold(Direction::new(i16::MAX, 0).expect("east"));
    let outcome = step(&body, &Absent);
    assert_eq!(outcome.at, body.at(), "unknown ground fails closed");
}

#[test]
fn a_footprint_is_tested_over_the_whole_body() {
    let ground = Patch { blocked: &[(5, 4)] };
    let from = cell_at(centre(4, 4));
    // A point at the cell's east edge is clear; a body wide enough to reach
    // into the blocked cell is not.
    let edge = WorldPoint {
        x: 5 * CELL_SUB_UNITS - 33,
        y: centre(4, 4).y,
    };
    assert!(footprint_clear(&ground, from, edge, 1));
    assert!(
        !footprint_clear(&ground, from, edge, 64),
        "a body wide enough to reach the blocked cell is refused"
    );
}

#[test]
fn bodies_that_do_not_touch_need_no_separation() {
    let a = WorldPoint { x: 0, y: 0 };
    let b = WorldPoint { x: 1_000, y: 0 };
    assert_eq!(separation(a, 100, b, 100), None);
    // Exactly in contact is not overlapping.
    let touching = WorldPoint { x: 200, y: 0 };
    assert_eq!(separation(a, 100, touching, 100), None);
}

#[test]
fn an_overlap_pushes_far_enough_to_part() {
    let a = WorldPoint { x: 0, y: 0 };
    let b = WorldPoint { x: 100, y: 0 };
    let (dx, dy) = separation(a, 100, b, 100).expect("they overlap");
    assert!(dx < 0, "the near body moves away from the far one");
    assert_eq!(dy, 0);
    // Each takes the same push in opposite directions, and two halves cover
    // the whole overlap.
    assert!(-dx * 2 >= 200 - 100, "the pair must actually part");
}

#[test]
fn coincident_bodies_part_along_a_stated_axis() {
    let same = WorldPoint { x: 42, y: 42 };
    let (dx, dy) = separation(same, 100, same, 100).expect("they overlap");
    assert!(dx < 0 && dy == 0, "the first-named body goes west");
    let repeat = separation(same, 100, same, 100).expect("they overlap");
    assert_eq!((dx, dy), repeat, "and does so identically every time");
}

#[test]
fn separation_at_world_extremes_neither_overflows_nor_touches() {
    let west = WorldPoint {
        x: i32::MIN,
        y: i32::MIN,
    };
    let east = WorldPoint {
        x: i32::MAX,
        y: i32::MAX,
    };
    assert_eq!(separation(west, u16::MAX, east, u16::MAX), None);
}
