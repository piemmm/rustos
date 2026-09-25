//! The shipped motions measured against the bounds beside the measurements.

use tairix_util::mathf;

use super::{
    closure, continuity, grounding, limits, penetration, skate, Measured, MAX_CLOSURE,
    MAX_CONTINUITY, MAX_GROUNDING, MAX_LIMIT_USE, MAX_SKATE,
};
use crate::clip::{Clip, Curve, Easing, Key, Lift, Loop};
use crate::gait::Gait;
use crate::humanoid::{self, DRIVES};
use crate::motion::{Kind, Motion, Support};
use crate::plant::Legs;
use crate::pose::Param;
use crate::rigging::Rigging;
use crate::socket::Side;
use crate::species::Species;
use crate::testing::{corners, human};

/// The whole point of the item: the art's quality is these numbers, and they
/// are held here rather than looked at.
#[test]
fn every_shipped_motion_clears_every_bound() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");

    for kind in Kind::ALL {
        let motion = Motion::new(kind).expect("a shipped motion");
        let clip = motion.clip().expect("its clip");
        let measured = Measured::of(kind, &rigging, clip, &legs).expect("it measures");
        assert_eq!(
            measured.breach(),
            None,
            "{} breaches a bound: {measured:?}",
            kind.name()
        );
    }
}

/// Which measurements a motion is held to follows from what it is, and each
/// is held to its own bound.
#[test]
fn a_motion_is_held_to_what_it_is() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");

    for kind in Kind::ALL {
        let motion = Motion::new(kind).expect("a shipped motion");
        let clip = motion.clip().expect("its clip");
        let measured = Measured::of(kind, &rigging, clip, &legs).expect("it measures");
        let name = kind.name();
        assert_eq!(
            measured.closure.is_some(),
            clip.repeat() == Loop::Wrap,
            "{name}"
        );
        assert_eq!(measured.skate.is_some(), kind.stride().is_some(), "{name}");
        let grounded = kind.support() == Support::Ground;
        assert_eq!(measured.grounding.is_some(), grounded, "{name}");
        assert_eq!(measured.penetration.is_some(), !grounded, "{name}");
        for (what, value, bound) in measured.each() {
            let expected = match what {
                "limits" => MAX_LIMIT_USE,
                "continuity" => MAX_CONTINUITY,
                "closure" => MAX_CLOSURE,
                "grounding" | "penetration" => MAX_GROUNDING,
                "skate" => MAX_SKATE,
                other => panic!("{name} measured an unknown {other}"),
            };
            assert!(
                mathf::fabs(bound - expected) < f64::EPSILON,
                "{name} {what}"
            );
            assert!(value.is_finite(), "{name} {what}");
        }
    }
    // The single measurements the record is made from are the ones the
    // module exports.
    let walk = Motion::new(Kind::Walk).expect("the shipped walk");
    let clip = walk.clip().expect("its clip");
    let measured = Measured::of(Kind::Walk, &rigging, clip, &legs).expect("it measures");
    let bits = |value: Option<f64>| value.map(f64::to_bits);
    assert_eq!(
        measured.limits.to_bits(),
        limits(&rigging, clip).expect("it measures").to_bits()
    );
    assert_eq!(measured.continuity.to_bits(), continuity(clip).to_bits());
    assert_eq!(bits(measured.closure), bits(Some(closure(clip))));
    assert_eq!(
        bits(measured.grounding),
        bits(Some(grounding(&rigging, clip, &legs).expect("it measures")))
    );
    assert_eq!(
        bits(measured.skate),
        bits(Some(
            skate(&rigging, clip, &legs, Side::Left).expect("it measures")
        ))
    );
}

/// A figure with nothing under it may not put a foot through the floor, and
/// the measurement sees one that does.
#[test]
fn penetration_sees_a_foot_through_the_floor() {
    const STRAIGHT: [Key; 1] = [Key::new(0.0, 0.0)];
    const DOWN: [Key; 2] = [Key::new(0.0, -0.2), Key::new(1.0, -0.2)];
    const UP: [Key; 2] = [Key::new(0.0, 0.2), Key::new(1.0, 0.2)];
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let curves = [Curve::new(Param::KneeBend(Side::Left), &STRAIGHT).expect("a curve")];

    let standing = Clip::new(1.0, Loop::Wrap, &curves, &[]).expect("a clip");
    let at_rest = penetration(&rigging, standing, &legs).expect("it measures");
    assert!(
        mathf::fabs(at_rest) < 1e-9,
        "a straight leg at rest penetrates {at_rest}"
    );

    let sunk = standing
        .lifting(Lift::new(&DOWN).expect("a lift"))
        .expect("it closes");
    let depth = penetration(&rigging, sunk, &legs).expect("it measures");
    assert!(
        mathf::fabs(depth - 0.2 * legs.straight()) < 1e-9,
        "a body sunk a fifth of a leg penetrates {depth}"
    );

    let raised = standing
        .lifting(Lift::new(&UP).expect("a lift"))
        .expect("it closes");
    let clear = penetration(&rigging, raised, &legs).expect("it measures");
    assert!(
        mathf::fabs(clear) < 1e-12,
        "a body off the ground penetrates {clear}"
    );
}

/// Grounding is two defects in one number, and it must catch both: a clip
/// whose root sits too low drives its foot into the floor, and one whose
/// root sits too high never lands at all. Neither is distinguishable from
/// the articulation, which is why the height is authored and then measured.
#[test]
fn grounding_catches_a_root_too_low_and_a_root_too_high() {
    const FOLDED: [Key; 1] = [Key::new(0.0, 0.35)];
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    // Legs folded and no root height at all: the figure hovers by its fold.
    let curves = [
        Curve::new(Param::KneeBend(Side::Left), &FOLDED).expect("a real curve"),
        Curve::new(Param::KneeBend(Side::Right), &FOLDED).expect("a real curve"),
    ];
    let hovering = Clip::new(1.0, Loop::Wrap, &curves, &[]).expect("a real clip");
    let hover = grounding(&rigging, hovering, &legs).expect("it resolves");
    assert!(
        hover > MAX_GROUNDING,
        "a crouch with no root height must not pass at {hover}"
    );

    // The same legs with the root driven further down than the fold: now the
    // foot is through the floor by as much again.
    let sunk_by = 2.0;
    let lift = [
        Key::new(0.0, -(hover + sunk_by) / legs.straight()),
        Key::new(1.0, -(hover + sunk_by) / legs.straight()),
    ];
    let sinking = hovering
        .lifting(crate::clip::Lift::new(&lift).expect("a real lift"))
        .expect("a closing lift");
    let sunk = grounding(&rigging, sinking, &legs).expect("it resolves");
    assert!(
        mathf::fabs(sunk - sunk_by) < 1e-6,
        "a root {sunk_by} past the fold sinks {sunk}"
    );
}

/// A clip pinned at a joint's limit measures one, and one is past the bound
/// — so the bound is a statement about headroom rather than a restatement of
/// what the drive table already guarantees.
#[test]
fn a_clip_at_a_joints_limit_measures_its_whole_travel() {
    const FOLDED: [Key; 1] = [Key::new(0.0, 1.0)];
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let curves = [Curve::new(Param::ElbowBend(Side::Left), &FOLDED).expect("a real curve")];
    let clip = Clip::new(1.0, Loop::Wrap, &curves, &[]).expect("a real clip");

    let used = limits(&rigging, clip).expect("a folded elbow is still posturable");
    assert!(mathf::fabs(used - 1.0) < 1e-12, "measured {used}");
    assert!(used > MAX_LIMIT_USE, "the bound must reject a pinned joint");
}

/// A step in a curve is a pop, and a pop is what the continuity bound is
/// for: the measurement must see it at the size it is, not a fraction of it.
#[test]
fn a_step_in_a_curve_reads_as_the_pop_it_is() {
    const STEPPED: [Key; 4] = [
        Key::new(0.0, 0.0),
        Key::new(0.5, 0.0).eased(Easing::Hold),
        Key::new(0.5625, 0.5),
        Key::new(1.0, 0.0),
    ];
    let curves = [Curve::new(Param::SpineBend, &STEPPED).expect("a real curve")];
    let clip = Clip::new(1.0, Loop::Wrap, &curves, &[]).expect("a real clip");
    let bend = continuity(clip);
    assert!(
        bend > MAX_CONTINUITY,
        "a half-range step measured only {bend}"
    );
}

/// The body's height is authored like any curve and can pop like one: a
/// root height that steps is caught at the size it is, and one that holds
/// still reads as nothing at all.
#[test]
fn a_step_in_the_root_height_reads_as_the_pop_it_is() {
    const HELD: [Key; 2] = [Key::new(0.0, -0.1), Key::new(1.0, -0.1)];
    const STEPPED: [Key; 4] = [
        Key::new(0.0, -0.1),
        Key::new(0.5, -0.1).eased(Easing::Hold),
        Key::new(0.5625, 0.1),
        Key::new(1.0, -0.1),
    ];
    let clip = Clip::new(1.0, Loop::Wrap, &[], &[]).expect("a real clip");
    let held = clip
        .lifting(Lift::new(&HELD).expect("a real lift"))
        .expect("a closing lift");
    assert!(
        continuity(held) < 1e-12,
        "a still body bent {}",
        continuity(held)
    );
    let stepped = clip
        .lifting(Lift::new(&STEPPED).expect("a real lift"))
        .expect("a closing lift");
    let bend = continuity(stepped);
    assert!(
        bend > MAX_CONTINUITY,
        "a fifth of a leg's jump in the body measured only {bend}"
    );
}

/// A cycle whose ends do not meet hitches once a lap, and the closure
/// measurement is what sees it.
#[test]
fn a_cycle_that_does_not_join_is_reported() {
    const OPEN: [Key; 2] = [Key::new(0.0, -0.4), Key::new(1.0, 0.4)];
    let curves = [Curve::new(Param::SpineTwist, &OPEN).expect("a real curve")];
    let clip = Clip::new(1.0, Loop::Wrap, &curves, &[]).expect("a real clip");
    let gap = closure(clip);
    assert!(
        mathf::fabs(gap - 0.4) < 1e-12,
        "an eight-tenths gap over a two-wide range measured {gap}"
    );
    assert!(gap > MAX_CLOSURE);
}

/// A clip whose foot keeps its own cadence while the body moves at another
/// skates, and the measurement is what turns that into a number.
#[test]
fn a_foot_out_of_step_with_the_body_skates() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let motion = Motion::new(Kind::Walk).expect("the shipped walk");
    let clip = motion.clip().expect("its clip");
    let honest = skate(&rigging, clip, &legs, Side::Left).expect("a fitted gait");

    let fitted = Gait::fitted(&rigging, clip, &legs, Side::Left).expect("a fitted gait");
    let gait = Gait::new(fitted.stride() * 0.6).expect("a gait");
    let forced = gait
        .slide(&rigging, clip, &legs, Side::Left)
        .expect("a slide")
        / gait.stride();
    assert!(
        forced > honest * 10.0,
        "a stride four tenths short skated {forced} against {honest}"
    );
}

/// A clip names no length and no joint, so it plays on every build: at every
/// build corner of every species the shipped motions stay grounded and do
/// not skate, measured by the same code and held to the same bounds as the
/// reference figure.
#[test]
fn every_shipped_motion_clears_its_bounds_on_every_build() {
    let motions = Kind::ALL.map(|kind| Motion::new(kind).expect("a shipped motion"));
    for species in Species::ALL {
        for identity in corners(species) {
            let rig = humanoid::rig(&identity).expect("every build builds");
            let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
            let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
            for motion in &motions {
                let clip = motion.clip().expect("its clip");
                let measured =
                    Measured::of(motion.kind(), &rigging, clip, &legs).expect("measurable");
                assert_eq!(
                    measured.breach(),
                    None,
                    "{species:?} {} at {:?}: {measured:?}",
                    motion.kind().name(),
                    identity.spec().build
                );
            }
        }
    }
}
