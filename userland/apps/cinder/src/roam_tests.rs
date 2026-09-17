//! Roaming tests: that he goes somewhere, arrives, stops, and turns at an
//! edge instead of walking on the spot.
//!
//! These are the tests the `Run` binary could not have. Every defect they
//! close was visible to anyone watching the desktop and invisible to the whole
//! pipeline.

use super::{Roam, ARRIVED};
use crate::mind::{Intent, Mind, Needs};
use crate::project::{ground_distance, Ground};
use crate::world::{Area, Plate, World};
use tairix_abi::window_ipc::TerrainPlate;
use tairix_rng::FastRng;

/// A frame at the rate the run loop animates.
const DT: f64 = 1.0 / 30.0;

fn desktop(area: Area) -> World {
    let mut world = World::new();
    world.set_area(area);
    world
}

fn screen() -> World {
    desktop(Area {
        left: 0,
        top: 0,
        right: 1280,
        bottom: 800,
    })
}

fn mind() -> Mind<FastRng> {
    Mind::new(FastRng::seed_from_u64(0x5EED_C1AD_1234_5678))
}

/// A mind with `needs`, so a test can pin which intent gets chosen.
///
/// The chooser takes the lowest need below its pressing threshold, so one
/// clearly-lowest need selects an intent without a draw deciding anything.
fn minded(needs: Needs, seed: u64) -> Mind<FastRng> {
    Mind::resuming(FastRng::seed_from_u64(seed), needs)
}

/// A mind that keeps choosing `ComeHome`: wanting company above all.
fn homesick() -> Mind<FastRng> {
    minded(
        Needs {
            energy: 1.0,
            play: 0.9,
            affection: 0.05,
        },
        0x0A11_0C1A_9999_0001,
    )
}

/// A mind that keeps choosing `Wander`: wanting play above all.
fn restless() -> Mind<FastRng> {
    minded(
        Needs {
            energy: 1.0,
            play: 0.05,
            affection: 0.9,
        },
        0x3A17_0B0B_4242_0007,
    )
}

/// Run `frames` frames, answering where he ended up.
fn run(roam: &mut Roam, mind: &mut Mind<FastRng>, world: &World, frames: usize) -> Ground {
    for _ in 0..frames {
        roam.advance(mind, world, DT);
    }
    roam.at()
}

#[test]
fn a_fresh_companion_is_at_home_where_he_was_let_out() {
    let at = Ground::new(640.0, 480.0);
    let roam = Roam::new(at);
    assert_eq!(roam.at(), at);
    assert_eq!(
        roam.home(),
        at,
        "the place he came out is the only home the desktop gave him"
    );
}

#[test]
fn walking_home_from_away_arrives_and_then_stops() {
    // The defect: home was his own current position, so the destination
    // cleared and re-set every frame, `heading_towards(at, at)` answered zero,
    // and he drove off screen-right for good. A real home must be reachable,
    // reached, and the end of the journey.
    let world = screen();
    let mut roam = Roam::new(Ground::new(640.0, 420.0));
    let home = roam.home();

    // Wander him away first, which is the only way the app itself produces a
    // companion standing somewhere other than where he came out.
    let mut restless = restless();
    let away = run(&mut roam, &mut restless, &world, 900);
    assert!(
        ground_distance(away, home) > ARRIVED * 3.0,
        "the walk out must actually leave home ({away:?} against {home:?})"
    );

    let mut homesick = homesick();
    let arrived = run(&mut roam, &mut homesick, &world, 1_800);
    assert!(
        ground_distance(arrived, home) < ARRIVED * 2.0,
        "he must actually get home: {arrived:?} against {home:?}"
    );

    // And having got there he settles, rather than walking through it.
    let settled = run(&mut roam, &mut homesick, &world, 300);
    assert!(
        ground_distance(settled, home) < ARRIVED * 2.0,
        "he must stay home once he is home: {settled:?}"
    );
}

#[test]
fn standing_at_his_destination_stops_the_legs() {
    // The gait is driven by the ground he covered, so arriving must report no
    // travel at all. Reporting the intended pace is what cycled his legs on
    // the spot.
    let world = screen();
    let mut roam = Roam::new(Ground::new(640.0, 400.0));
    let mut mind = homesick();
    // He starts at home, so a walk home has nowhere to go.
    for _ in 0..30 {
        let stepped = roam.advance(&mut mind, &world, DT);
        if stepped.intent != Intent::ComeHome {
            continue;
        }
        assert!(
            stepped.travelled < f64::EPSILON,
            "a companion standing at home reported travelling {}",
            stepped.travelled
        );
    }
}

#[test]
fn standing_on_an_edge_he_is_never_still_facing_out_of_it() {
    // The defect: the work area's clamp held his position while the gait was
    // told he was running, so he walked on the spot pressed to the boundary
    // for ever. An edge deflects him, so a companion who has just met one is
    // already facing back inside.
    let area = Area {
        left: 0,
        top: 0,
        right: 200,
        bottom: 200,
    };
    let world = desktop(area);
    let mut roam = Roam::new(Ground::new(100.0, 100.0));
    let mut mind = mind();
    for _ in 0..3_000 {
        roam.advance(&mut mind, &world, DT);
        let at = roam.at();
        let heading = roam.heading();
        let (dx, dy) = crate::project::step(heading, 1.0);
        if at.x <= f64::from(area.left) {
            assert!(dx >= -1.0e-9, "on the left edge still heading left ({dx})");
        }
        if at.x >= f64::from(area.right - 1) {
            assert!(dx <= 1.0e-9, "on the right edge still heading right ({dx})");
        }
        if at.y <= f64::from(area.top) {
            assert!(dy >= -1.0e-9, "on the top edge still heading up ({dy})");
        }
        if at.y >= f64::from(area.bottom - 1) {
            assert!(dy <= 1.0e-9, "on the bottom edge still heading down ({dy})");
        }
    }
}

#[test]
fn he_never_leaves_the_work_area_however_long_he_roams() {
    let area = Area {
        left: 10,
        top: 20,
        right: 400,
        bottom: 300,
    };
    let world = desktop(area);
    let mut roam = Roam::new(Ground::new(200.0, 150.0));
    let mut mind = mind();
    for _ in 0..3_000 {
        roam.advance(&mut mind, &world, DT);
        let at = roam.at();
        assert!(
            at.x >= f64::from(area.left)
                && at.x <= f64::from(area.right - 1)
                && at.y >= f64::from(area.top)
                && at.y <= f64::from(area.bottom - 1),
            "he wandered to {at:?}, outside {area:?}"
        );
    }
}

#[test]
fn travel_reported_is_the_ground_he_actually_covered() {
    let world = screen();
    let mut roam = Roam::new(Ground::new(640.0, 400.0));
    let mut mind = mind();
    for _ in 0..600 {
        let before = roam.at();
        let stepped = roam.advance(&mut mind, &world, DT);
        let covered = ground_distance(before, roam.at()) / DT;
        assert!(
            (stepped.travelled - covered).abs() < 1.0e-9,
            "reported {} but moved {covered}",
            stepped.travelled
        );
    }
}

#[test]
fn a_resting_intent_neither_walks_nor_holds_a_destination() {
    let world = screen();
    let mut roam = Roam::new(Ground::new(640.0, 400.0));
    let mut mind = mind();
    for _ in 0..900 {
        let stepped = roam.advance(&mut mind, &world, DT);
        if !matches!(stepped.intent, Intent::Sit | Intent::Groom | Intent::Nap) {
            continue;
        }
        assert_eq!(roam.target(), None, "a resting companion goes nowhere");
        assert!(
            stepped.travelled < f64::EPSILON,
            "a resting companion travelled {}",
            stepped.travelled
        );
    }
}

#[test]
fn a_chase_closes_on_the_pointer_and_settles_on_reaching_it() {
    let world = screen();
    let at = Ground::new(200.0, 600.0);
    let mut roam = Roam::new(at);
    // Inside the range the mind notices a pointer at all, and with both the
    // play and company needs pressing so the chase is taken rather than drawn
    // for.
    let mut mind = minded(
        Needs {
            energy: 1.0,
            play: 0.05,
            affection: 0.05,
        },
        0xC4A5_E000_0001_0002,
    );
    let pointer = Ground::new(460.0, 560.0);
    assert!(
        ground_distance(at, pointer) < crate::mind::NOTICE_RANGE,
        "the mind only chases a pointer it can notice"
    );
    roam.see_pointer(Some(pointer));
    let mut closest = ground_distance(roam.at(), pointer);
    for _ in 0..600 {
        // Judged on where he stood when the frame began: the frame that
        // *crosses* into range covers ground legitimately, and it is the one
        // after it that must not.
        let already_there = ground_distance(roam.at(), pointer) < ARRIVED;
        let stepped = roam.advance(&mut mind, &world, DT);
        closest = closest.min(ground_distance(roam.at(), pointer));
        if matches!(stepped.intent, Intent::Chase | Intent::Pounce) && already_there {
            assert!(
                stepped.travelled < f64::EPSILON,
                "he kept running at a pointer he had already reached"
            );
        }
    }
    assert!(
        closest < ARRIVED * 2.0,
        "a chase must actually close on the pointer (got within {closest})"
    );
}

#[test]
fn an_approach_walks_to_the_plate_rather_than_inheriting_a_stale_destination() {
    // A crossing that kept whatever destination the last wander left would
    // stall short of the window it was crossing and never complete.
    let mut world = screen();
    let plate = Plate {
        left: 500,
        top: 380,
        right: 800,
        bottom: 560,
    };
    world.adopt_terrain(&[TerrainPlate {
        x: plate.left,
        y: plate.top,
        width_px: u32::try_from(plate.right - plate.left).expect("a positive width"),
        height_px: u32::try_from(plate.bottom - plate.top).expect("a positive height"),
    }]);
    let mut roam = Roam::new(Ground::new(460.0, 470.0));
    let mut mind = mind();
    for _ in 0..1_200 {
        let stepped = roam.advance(&mut mind, &world, DT);
        if !matches!(stepped.intent, Intent::Climb | Intent::Burrow) {
            continue;
        }
        if let Some(target) = roam.target() {
            let nearest = plate.nearest(roam.at());
            assert!(
                ground_distance(target, nearest) < 1.0e-9 || roam.target() != Some(target),
                "an approach must aim at the plate, not at {target:?}"
            );
            return;
        }
    }
}

#[test]
fn a_frame_is_reproducible_from_the_same_state_and_seed() {
    // Every decision is drawn from an injected generator, so two runs from the
    // same seed must be identical — which is what makes any of this testable.
    let world = screen();
    let path = |seed: u64| {
        let mut roam = Roam::new(Ground::new(640.0, 400.0));
        let mut mind: Mind<FastRng> = Mind::new(FastRng::seed_from_u64(seed));
        let mut out = alloc::vec::Vec::new();
        for _ in 0..200 {
            roam.advance(&mut mind, &world, DT);
            out.push((roam.at().x, roam.at().y, roam.heading()));
        }
        out
    };
    assert_eq!(path(0x1234_5678_9ABC_DEF0), path(0x1234_5678_9ABC_DEF0));
}

#[test]
fn a_frame_of_no_time_moves_nothing_and_answers_no_rates() {
    // The rates are per-second and divide by the step. A zero would answer NaN
    // for both, and a NaN screen coordinate converts to zero — so the creature
    // would be drawn in the corner of his own surface rather than where he is.
    let world = screen();
    let mut mind = mind();
    for dt in [0.0, -1.0 / 30.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut roam = Roam::new(Ground::new(640.0, 400.0));
        let before = (roam.at(), roam.heading());
        let stepped = roam.advance(&mut mind, &world, dt);
        assert_eq!(roam.at(), before.0, "a {dt} step moved him");
        assert!((roam.heading() - before.1).abs() < f64::EPSILON);
        assert!(
            stepped.travelled.is_finite() && stepped.turn_rate.is_finite(),
            "a {dt} step answered travelled={} turn_rate={}",
            stepped.travelled,
            stepped.turn_rate
        );
        assert!(stepped.travelled.abs() < f64::EPSILON);
        assert!(stepped.turn_rate.abs() < f64::EPSILON);
    }
}

#[test]
fn every_frame_of_a_long_roam_keeps_the_pose_finite() {
    // Nothing the mind, the world or the route does may put a NaN into a
    // position: it converts to zero on the way to the screen, which is a
    // creature drawn in the corner rather than a visible failure.
    let world = screen();
    let mut roam = Roam::new(Ground::new(640.0, 400.0));
    let mut mind = mind();
    for _ in 0..3_000 {
        let stepped = roam.advance(&mut mind, &world, DT);
        assert!(roam.at().x.is_finite() && roam.at().y.is_finite());
        assert!(roam.heading().is_finite());
        assert!(stepped.travelled.is_finite() && stepped.turn_rate.is_finite());
        assert!(stepped.advanced.lift.is_finite() && stepped.advanced.crouch.is_finite());
    }
}
