//! World tests: the terrain model, the route state machine, and — the one
//! that matters most — when the stacking depth flips.

use super::{advance, choose_crossing, Area, Crossing, Plate, Route, World};
use crate::gait::{BURROW_SECONDS, JUMP_SECONDS};
use crate::project::Ground;
use tairix_abi::window_ipc::{LayerDepth, TerrainPlate};

fn wire(x: i32, y: i32, w: u32, h: u32) -> TerrainPlate {
    TerrainPlate {
        x,
        y,
        width_px: w,
        height_px: h,
    }
}

fn tall() -> Plate {
    Plate {
        left: 100,
        top: 100,
        right: 200,
        bottom: 300,
    }
}

fn wide() -> Plate {
    Plate {
        left: 100,
        top: 100,
        right: 400,
        bottom: 160,
    }
}

#[test]
fn a_wire_plate_with_no_area_is_not_terrain() {
    assert!(Plate::from_wire(wire(0, 0, 0, 10)).is_none());
    assert!(Plate::from_wire(wire(0, 0, 10, 0)).is_none());
    assert!(Plate::from_wire(wire(5, 5, 10, 10)).is_some());
}

#[test]
fn a_wire_plate_that_would_overflow_the_coordinate_space_is_refused() {
    assert!(
        Plate::from_wire(wire(i32::MAX - 1, 0, 100, 100)).is_none(),
        "a plate must be refused rather than wrapped"
    );
}

#[test]
fn adopting_terrain_drops_the_plates_that_name_no_area() {
    let mut world = World::new();
    world.adopt_terrain(&[wire(0, 0, 10, 10), wire(5, 5, 0, 0), wire(20, 20, 4, 4)]);
    assert_eq!(world.plates().len(), 2);
}

#[test]
fn adopting_terrain_replaces_rather_than_accumulates() {
    let mut world = World::new();
    world.adopt_terrain(&[wire(0, 0, 10, 10)]);
    world.adopt_terrain(&[wire(0, 0, 10, 10), wire(30, 30, 10, 10)]);
    assert_eq!(world.plates().len(), 2);
}

#[test]
fn the_frontmost_plate_is_the_one_underfoot() {
    let mut world = World::new();
    // Back to front; the one the user sees at a point is the last covering it.
    world.adopt_terrain(&[wire(0, 0, 100, 100), wire(50, 50, 100, 100)]);
    let under = world.plate_at(Ground::new(60.0, 60.0)).expect("on a plate");
    assert_eq!(under.left, 50, "the frontmost window wins");
}

#[test]
fn standing_on_nothing_reports_nothing() {
    let mut world = World::new();
    world.adopt_terrain(&[wire(0, 0, 10, 10)]);
    assert!(world.plate_at(Ground::new(500.0, 500.0)).is_none());
}

#[test]
fn an_obstacle_is_seen_a_stride_ahead_rather_than_underfoot() {
    let mut world = World::new();
    world.adopt_terrain(&[wire(100, 0, 100, 100)]);
    // Standing clear, walking right: the plate is ahead.
    let ahead = world.obstacle_ahead(Ground::new(80.0, 50.0), 0.0, 40.0);
    assert!(
        ahead.is_some(),
        "he must decide to climb before walking into it"
    );
    // Walking away from it there is nothing in the way.
    assert!(world
        .obstacle_ahead(Ground::new(80.0, 50.0), core::f64::consts::PI, 40.0)
        .is_none());
}

#[test]
fn the_plate_already_underfoot_is_not_an_obstacle() {
    let mut world = World::new();
    world.adopt_terrain(&[wire(0, 0, 400, 400)]);
    assert!(
        world
            .obstacle_ahead(Ground::new(100.0, 100.0), 0.0, 20.0)
            .is_none(),
        "what he is already on is not in his way"
    );
}

#[test]
fn a_tall_window_is_climbed_and_a_wide_one_is_gone_under() {
    // Boldness above the "walk around" share, so the geometry decides.
    assert_eq!(choose_crossing(tall(), 0.9), Crossing::Over);
    assert_eq!(choose_crossing(wide(), 0.9), Crossing::Under);
}

#[test]
fn sometimes_he_simply_walks_round() {
    assert_eq!(choose_crossing(tall(), 0.0), Crossing::Around);
}

#[test]
fn the_depth_flips_at_the_apex_of_a_climb_not_at_its_start() {
    // The whole reason the state machine owns the depth: flipping at take-off
    // would pop him in front of the window before he had left the floor.
    let start = Route::Climb {
        plate: tall(),
        elapsed: 0.0,
    };
    assert_eq!(start.depth(), LayerDepth::Below);
    let apex = Route::Climb {
        plate: tall(),
        elapsed: JUMP_SECONDS / 2.0,
    };
    assert_eq!(apex.depth(), LayerDepth::Above);
    let landing = Route::Climb {
        plate: tall(),
        elapsed: JUMP_SECONDS * 0.99,
    };
    assert_eq!(landing.depth(), LayerDepth::Above);
}

#[test]
fn the_depth_flips_back_at_the_apex_of_a_descent() {
    assert_eq!(Route::Descend { elapsed: 0.0 }.depth(), LayerDepth::Above);
    assert_eq!(
        Route::Descend {
            elapsed: JUMP_SECONDS / 2.0
        }
        .depth(),
        LayerDepth::Below
    );
}

#[test]
fn every_grounded_state_is_below_and_a_perch_is_above() {
    assert_eq!(Route::Floor.depth(), LayerDepth::Below);
    assert_eq!(
        Route::Burrow {
            plate: wide(),
            elapsed: 0.2
        }
        .depth(),
        LayerDepth::Below
    );
    assert_eq!(
        Route::Skirt {
            plate: wide(),
            clockwise: true
        }
        .depth(),
        LayerDepth::Below
    );
    assert_eq!(Route::Perch { plate: tall() }.depth(), LayerDepth::Above);
}

#[test]
fn an_approach_becomes_the_crossing_it_chose_once_he_arrives() {
    let world = World::new();
    let at_edge = Ground::new(100.0, 150.0);
    for (by, expected_airborne) in [(Crossing::Over, true), (Crossing::Under, false)] {
        let advanced = advance(
            Route::Approach { plate: tall(), by },
            at_edge,
            &world,
            0.016,
        );
        assert_eq!(advanced.route.is_airborne(), expected_airborne);
    }
}

#[test]
fn an_approach_that_has_not_arrived_stays_an_approach() {
    let world = World::new();
    let advanced = advance(
        Route::Approach {
            plate: tall(),
            by: Crossing::Over,
        },
        Ground::new(-500.0, -500.0),
        &world,
        0.016,
    );
    assert!(matches!(advanced.route, Route::Approach { .. }));
}

#[test]
fn a_climb_lands_on_the_plate_and_a_descent_lands_on_the_floor() {
    let world = World::new();
    let landed = advance(
        Route::Climb {
            plate: tall(),
            elapsed: JUMP_SECONDS,
        },
        Ground::new(150.0, 150.0),
        &world,
        0.016,
    );
    assert!(matches!(landed.route, Route::Perch { .. }));

    let down = advance(
        Route::Descend {
            elapsed: JUMP_SECONDS,
        },
        Ground::new(150.0, 400.0),
        &world,
        0.016,
    );
    assert_eq!(down.route, Route::Floor);
}

#[test]
fn stepping_off_a_perch_becomes_a_descent_rather_than_a_fall_through() {
    let world = World::new();
    let stepped = advance(
        Route::Perch { plate: tall() },
        Ground::new(500.0, 500.0),
        &world,
        0.016,
    );
    assert!(matches!(stepped.route, Route::Descend { .. }));
}

#[test]
fn staying_on_a_perch_stays_perched() {
    let world = World::new();
    let stayed = advance(
        Route::Perch { plate: tall() },
        Ground::new(150.0, 150.0),
        &world,
        0.016,
    );
    assert!(matches!(stayed.route, Route::Perch { .. }));
}

#[test]
fn a_burrow_flattens_then_ends_once_he_is_out_the_other_side() {
    let world = World::new();
    let mid = advance(
        Route::Burrow {
            plate: wide(),
            elapsed: BURROW_SECONDS,
        },
        Ground::new(150.0, 130.0),
        &world,
        0.016,
    );
    assert!(
        matches!(mid.route, Route::Burrow { .. }),
        "still underneath"
    );
    assert!(mid.crouch > 0.0, "and flattened while he is");

    let out = advance(
        Route::Burrow {
            plate: wide(),
            elapsed: BURROW_SECONDS + 0.1,
        },
        Ground::new(600.0, 600.0),
        &world,
        0.016,
    );
    assert_eq!(out.route, Route::Floor);
    assert!(out.crouch.abs() <= f64::EPSILON);
}

#[test]
fn a_climb_reports_a_depth_change_exactly_once_as_it_crosses_the_apex() {
    let world = World::new();
    let before_apex = JUMP_SECONDS / 2.0 - 0.02;
    let crossing = advance(
        Route::Climb {
            plate: tall(),
            elapsed: before_apex,
        },
        Ground::new(150.0, 150.0),
        &world,
        0.04,
    );
    assert!(
        crossing.depth_changed,
        "the session must be told at the moment it reads correctly"
    );
    let after = advance(crossing.route, Ground::new(150.0, 150.0), &world, 0.016);
    assert!(!after.depth_changed, "and not told again every frame after");
}

#[test]
fn only_a_leap_is_drawn_lifted() {
    let world = World::new();
    let grounded = advance(Route::Floor, Ground::new(0.0, 0.0), &world, 0.016);
    assert!(grounded.lift.abs() <= f64::EPSILON);
    let airborne = advance(
        Route::Climb {
            plate: tall(),
            elapsed: JUMP_SECONDS / 2.0,
        },
        Ground::new(150.0, 150.0),
        &world,
        0.0,
    );
    assert!(airborne.lift > 0.0);
}

#[test]
fn the_work_area_pulls_a_stray_creature_back() {
    let area = Area {
        left: 0,
        top: 20,
        right: 800,
        bottom: 600,
    };
    assert_eq!(
        area.clamp(Ground::new(-50.0, -50.0)),
        Ground::new(0.0, 20.0)
    );
    assert_eq!(
        area.clamp(Ground::new(5_000.0, 5_000.0)),
        Ground::new(799.0, 599.0)
    );
    assert_eq!(
        area.clamp(Ground::new(100.0, 100.0)),
        Ground::new(100.0, 100.0)
    );
}

#[test]
fn an_empty_work_area_moves_nobody() {
    let area = Area::default();
    assert!(area.is_empty());
    assert_eq!(area.clamp(Ground::new(7.0, 7.0)), Ground::new(7.0, 7.0));
}

// --- edges deflect rather than pin ---------------------------------------

/// The 200-square area the deflect tests reason about.
fn square() -> Area {
    Area {
        left: 0,
        top: 0,
        right: 200,
        bottom: 200,
    }
}

#[test]
fn a_vertical_edge_reflects_the_heading_across_it() {
    // Walking left out of the area comes back walking right, and the vertical
    // component is untouched: he turns off the wall, he does not stop at it.
    let area = square();
    let heading = core::f64::consts::PI - 0.4;
    let turned = area.deflect(heading, Ground::new(-10.0, 100.0));
    assert!(
        tairix_util::mathf::cos(turned) > 0.0,
        "a heading off the left edge must point back inside"
    );
    assert!(
        (tairix_util::mathf::sin(turned) - tairix_util::mathf::sin(heading)).abs() < 1.0e-9,
        "only the component that met the edge is reflected"
    );
}

#[test]
fn a_horizontal_edge_reflects_the_other_component() {
    let area = square();
    let heading = 0.7;
    let turned = area.deflect(heading, Ground::new(100.0, -10.0));
    assert!(tairix_util::mathf::sin(turned) < 0.0);
    assert!((tairix_util::mathf::cos(turned) - tairix_util::mathf::cos(heading)).abs() < 1.0e-9);
}

#[test]
fn a_corner_sends_him_back_the_way_he_came() {
    let area = square();
    let heading = 0.7;
    let turned = area.deflect(heading, Ground::new(-5.0, -5.0));
    let reversed = crate::project::wrap_angle(heading + core::f64::consts::PI);
    assert!((crate::project::turn_towards(turned, reversed)).abs() < 1.0e-9);
}

#[test]
fn a_step_that_stays_inside_is_not_deflected_at_all() {
    let area = square();
    for heading in [0.0, 0.9, 2.2, -1.7, 3.0] {
        assert!(
            (area.deflect(heading, Ground::new(100.0, 100.0)) - heading).abs() < 1.0e-9,
            "a step inside the area must not be turned"
        );
    }
}

#[test]
fn an_empty_area_deflects_nothing() {
    // Before the first work-area report there is no boundary to bounce off,
    // and inventing one would send him somewhere arbitrary.
    let empty = Area {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    assert!((empty.deflect(1.2, Ground::new(-50.0, -50.0)) - 1.2).abs() < 1.0e-9);
}
