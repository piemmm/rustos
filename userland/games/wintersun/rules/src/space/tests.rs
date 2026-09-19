use alloc::vec::Vec;

use tairix_wintersun_net::value::{EntityId, WorldPoint};

use super::{cell_of, BroadPhase, GridCell, GRID_CELL_SUB_UNITS};

fn at(x: i32, y: i32) -> WorldPoint {
    WorldPoint { x, y }
}

fn collect(phase: &BroadPhase, centre: WorldPoint, radius: i64) -> Vec<EntityId> {
    let mut found = Vec::new();
    phase.for_each_near(centre, radius, |id| found.push(id));
    found
}

fn populated(bodies: &[(u64, i32, i32)]) -> BroadPhase {
    let mut phase = BroadPhase::new();
    phase
        .rebuild(
            bodies
                .iter()
                .map(|&(id, x, y)| (EntityId(id), at(x, y)))
                .collect::<Vec<_>>()
                .into_iter(),
        )
        .expect("room");
    phase
}

#[test]
fn a_point_maps_to_its_cell_across_the_origin() {
    assert_eq!(cell_of(at(0, 0)), GridCell { row: 0, column: 0 });
    assert_eq!(
        cell_of(at(-1, -1)),
        GridCell {
            row: -1,
            column: -1
        },
        "flooring, so the lattice has no double-width cell at the origin"
    );
    assert_eq!(
        cell_of(at(GRID_CELL_SUB_UNITS, -GRID_CELL_SUB_UNITS)),
        GridCell { row: -1, column: 1 }
    );
}

#[test]
fn an_empty_phase_finds_nothing() {
    let phase = BroadPhase::new();
    assert!(phase.is_empty());
    assert_eq!(phase.len(), 0);
    assert!(collect(&phase, at(0, 0), 10_000).is_empty());
}

#[test]
fn a_query_finds_everything_inside_it_and_visits_in_one_order() {
    let bodies = [
        (5_u64, 0_i32, 0_i32),
        (3, GRID_CELL_SUB_UNITS / 2, 0),
        (9, -GRID_CELL_SUB_UNITS * 4, 0),
        (1, 0, GRID_CELL_SUB_UNITS),
    ];
    let phase = populated(&bodies);
    assert_eq!(phase.len(), 4);

    let near = collect(&phase, at(0, 0), 1);
    assert_eq!(
        near,
        [EntityId(3), EntityId(5)],
        "row-major, then by identity"
    );

    let wider = collect(&phase, at(0, 0), i64::from(GRID_CELL_SUB_UNITS));
    assert_eq!(wider, [EntityId(3), EntityId(5), EntityId(1)]);
    assert!(!wider.contains(&EntityId(9)), "four cells away is outside");
}

#[test]
fn the_visit_order_does_not_depend_on_insertion_order() {
    let forward = [(1_u64, 0_i32, 0_i32), (2, 10, 10), (3, 20, 20)];
    let backward = [(3_u64, 20_i32, 20_i32), (2, 10, 10), (1, 0, 0)];
    let radius = i64::from(GRID_CELL_SUB_UNITS) * 4;
    assert_eq!(
        collect(&populated(&forward), at(0, 0), radius),
        collect(&populated(&backward), at(0, 0), radius),
        "an authoritative simulation cannot read an insertion order"
    );
}

#[test]
fn rebuilding_replaces_rather_than_accumulates() {
    let mut phase = populated(&[(1, 0, 0), (2, 0, 0)]);
    phase
        .rebuild([(EntityId(7), at(0, 0))].into_iter())
        .expect("room");
    assert_eq!(phase.len(), 1);
    assert_eq!(collect(&phase, at(0, 0), 1), [EntityId(7)]);
}

#[test]
fn a_query_spanning_the_origin_is_not_split_by_the_sign() {
    let bodies = [
        (1_u64, -GRID_CELL_SUB_UNITS, -GRID_CELL_SUB_UNITS),
        (2, -1, -1),
        (3, 1, 1),
        (4, GRID_CELL_SUB_UNITS, GRID_CELL_SUB_UNITS),
    ];
    let phase = populated(&bodies);
    let found = collect(&phase, at(0, 0), i64::from(GRID_CELL_SUB_UNITS) * 2);
    assert_eq!(found.len(), 4, "a signed column index must still order");
}

#[test]
fn a_query_wider_than_the_population_takes_the_linear_path_and_agrees() {
    let bodies = [
        (1_u64, -5_000_i32, -5_000_i32),
        (2, 0, 0),
        (3, 5_000, 5_000),
    ];
    let phase = populated(&bodies);
    // Wide enough to span more rows than there are bodies, which is the
    // path that walks the array instead of searching per row.
    let wide = collect(&phase, at(0, 0), 1_000_000);
    let narrow_each: Vec<EntityId> = [(-5_000, -5_000), (0, 0), (5_000, 5_000)]
        .into_iter()
        .flat_map(|(x, y)| collect(&phase, at(x, y), 1))
        .collect();
    assert_eq!(wide, [EntityId(1), EntityId(2), EntityId(3)]);
    assert_eq!(narrow_each, [EntityId(1), EntityId(2), EntityId(3)]);
}

#[test]
fn an_extreme_radius_neither_overflows_nor_loops_forever() {
    let phase = populated(&[(1, i32::MAX, i32::MAX), (2, i32::MIN, i32::MIN)]);
    let found = collect(&phase, at(0, 0), i64::MAX);
    assert_eq!(found.len(), 2);
    let none = collect(&phase, at(i32::MIN, i32::MIN), 0);
    assert_eq!(none, [EntityId(2)]);
}
