//! What a gait advances, and what it measures.

use tairix_util::mathf;

use super::Gait;
use crate::clip::{Clip, Curve, Event, Key, Lift, Loop};
use crate::error::FigureError;
use crate::frame::Body;
use crate::humanoid::{self, DRIVES, SHANK_LENGTH, THIGH_LENGTH};
use crate::plant::{solve, Legs};
use crate::pose::Param;
use crate::rigging::Rigging;
use crate::socket::Side;
use crate::testing::human;

const SLACK: f64 = 1e-9;

fn close(a: f64, b: f64) -> bool {
    mathf::fabs(a - b) <= SLACK
}

/// The left leg of a walk whose planted foot is level and moves backward
/// through the body at a constant rate.
///
/// Authored by solving the leg for a foot that tracks the ground from eight
/// units in front to eight behind, then arcs twelve units over to come
/// forward again — so the clip *is* a walk rather than a shape that looks
/// like one, and what the stride fitting recovers is a number this fixture
/// already knows: sixteen units of stance travel over half a cycle is a
/// stride of thirty-two.
const STRIDE: f64 = 32.0;

const HIP_KEYS: [Key; 9] = [
    Key::new(0.0, 0.2082),
    Key::new(0.125, 0.1871),
    Key::new(0.25, 0.1496),
    Key::new(0.375, 0.0985),
    Key::new(0.5, 0.0322),
    Key::new(0.625, 0.2908),
    Key::new(0.75, 0.4071),
    Key::new(0.875, 0.4),
    Key::new(1.0, 0.2082),
];

const KNEE_KEYS: [Key; 9] = [
    Key::new(0.0, 0.1961),
    Key::new(0.125, 0.2329),
    Key::new(0.25, 0.244),
    Key::new(0.375, 0.2329),
    Key::new(0.5, 0.1961),
    Key::new(0.625, 0.5615),
    Key::new(0.75, 0.6605),
    Key::new(0.875, 0.5615),
    Key::new(1.0, 0.1961),
];

const STEP: [Event; 0] = [];

fn curves() -> [Curve<'static>; 2] {
    [
        Curve::new(Param::HipSwing(Side::Left), &HIP_KEYS).expect("a real hip curve"),
        Curve::new(Param::KneeBend(Side::Left), &KNEE_KEYS).expect("a real knee curve"),
    ]
}

fn walk<'a>(curves: &'a [Curve<'a>]) -> Clip<'a> {
    Clip::new(1.0, Loop::Wrap, curves, &STEP).expect("a real walk")
}

#[test]
fn an_unreal_stride_is_refused() {
    for stride in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert_eq!(
            Gait::new(stride).map(|_| ()),
            Err(FigureError::StrideUnreal),
            "a stride of {stride} must be refused"
        );
    }
}

#[test]
fn an_unreal_distance_or_phase_is_refused() {
    let mut gait = Gait::new(10.0).expect("a real gait");
    for distance in [f64::NAN, f64::INFINITY] {
        assert_eq!(
            gait.travel(distance).map(|_| ()),
            Err(FigureError::DistanceUnreal)
        );
    }
    for phase in [-0.1, 1.1, f64::NAN] {
        assert_eq!(
            gait.set_phase(phase),
            Err(FigureError::PhaseOutsideClip),
            "a phase of {phase} must be refused"
        );
    }
    assert!(close(gait.phase(), 0.0), "a refusal moves nothing");
}

/// The whole point: the phase is a function of where the figure is, so the
/// same ground covered gives the same phase however it was divided up.
#[test]
fn the_phase_follows_distance_and_not_the_number_of_steps() {
    let mut whole = Gait::new(STRIDE).expect("a real gait");
    let mut split = Gait::new(STRIDE).expect("a real gait");
    whole.travel(53.0).expect("it travels");
    for _ in 0..53 {
        split.travel(1.0).expect("it travels");
    }
    assert!(mathf::fabs(whole.phase() - split.phase()) < 1e-12);
}

#[test]
fn a_figure_standing_still_does_not_walk_on_the_spot() {
    let mut gait = Gait::new(STRIDE).expect("a real gait");
    gait.travel(7.5).expect("it travels");
    let held = gait.phase();
    for _ in 0..100 {
        let stepped = gait.travel(0.0).expect("it travels");
        assert_eq!(stepped.cycles, 0);
        assert!(close(stepped.from, held) && close(stepped.to, held));
    }
    assert!(close(gait.phase(), held));
}

#[test]
fn a_known_distance_completes_a_known_number_of_cycles() {
    let mut gait = Gait::new(STRIDE).expect("a real gait");
    let stepped = gait.travel(STRIDE * 3.25).expect("it travels");
    assert_eq!(stepped.cycles, 3);
    assert!(close(stepped.from, 0.0));
    assert!(close(stepped.to, 0.25));
}

#[test]
fn walking_backward_runs_the_cycle_backward() {
    let mut gait = Gait::new(STRIDE).expect("a real gait");
    gait.travel(STRIDE * 0.25).expect("it travels");
    let stepped = gait.travel(-STRIDE * 0.5).expect("it travels");
    assert!(close(stepped.from, 0.25));
    assert!(close(stepped.to, 0.75), "phase {} wrapped", stepped.to);
    assert_eq!(stepped.cycles, 1, "a backward lap is still a lap crossed");
}

/// The phase must stay in the half-open cycle a clip is sampled over, however
/// many strides it has been carried through.
#[test]
fn the_phase_stays_inside_the_cycle_over_a_long_walk() {
    let mut gait = Gait::new(STRIDE).expect("a real gait");
    let mut travelled = 0.0;
    for step in 0..2_000 {
        let distance = f64::from(step % 17) * 0.37 - 1.0;
        travelled += distance;
        let stepped = gait.travel(distance).expect("it travels");
        assert!(
            (0.0..1.0).contains(&gait.phase()),
            "phase {} left the cycle after {travelled}",
            gait.phase()
        );
        assert!((0.0..1.0).contains(&stepped.to));
    }
}

/// The measurement the whole design rests on: the stride recovered from the
/// clip and the rig is the one the clip was built around, and the planted
/// foot then barely moves over the ground.
#[test]
fn a_fitted_stride_recovers_the_walk_it_was_authored_with() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let curves = curves();
    let walk = walk(&curves);

    let gait = Gait::fitted(&rigging, walk, &legs, Side::Left).expect("a fitted gait");
    assert!(
        mathf::fabs(gait.stride() - STRIDE) < 0.05 * STRIDE,
        "fitted stride {} should recover {STRIDE}",
        gait.stride()
    );

    let slide = gait
        .slide(&rigging, walk, &legs, Side::Left)
        .expect("a measured slide");
    assert!(
        slide < 0.01 * STRIDE,
        "a planted foot slid {slide} over a stride of {STRIDE}"
    );
}

/// Fitting has to *earn* that number: a stride that is not the clip's own
/// makes the same foot skate, which is the failure the fitting exists to
/// remove.
#[test]
fn a_stride_that_is_not_the_clips_own_makes_the_foot_skate() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let curves = curves();
    let walk = walk(&curves);

    let fitted = Gait::fitted(&rigging, walk, &legs, Side::Left).expect("a fitted gait");
    let honest = fitted
        .slide(&rigging, walk, &legs, Side::Left)
        .expect("a measured slide");
    for wrong in [0.5, 1.5, 2.0] {
        let guess = Gait::new(fitted.stride() * wrong).expect("a real gait");
        let skate = guess
            .slide(&rigging, walk, &legs, Side::Left)
            .expect("a measured slide");
        assert!(
            skate > honest * 10.0,
            "a stride {wrong} times the clip's own slid {skate} against {honest}"
        );
    }
}

/// A clip whose foot never leaves the ground is not a walk, and answering a
/// stride for it would be inventing one.
#[test]
fn a_clip_whose_foot_never_lifts_has_no_stride() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let flat = [Key::new(0.0, 0.2), Key::new(1.0, 0.2)];
    let curves = [Curve::new(Param::HipSwing(Side::Left), &flat).expect("a real curve")];
    let standing = Clip::new(1.0, Loop::Wrap, &curves, &STEP).expect("a real clip");
    assert_eq!(
        Gait::fitted(&rigging, standing, &legs, Side::Left).map(|_| ()),
        Err(FigureError::StrideUnreal)
    );
}

/// The window is measured cyclically, so the same walk rotated to start
/// mid-stance measures the same stride rather than two half windows.
#[test]
fn a_walk_authored_from_mid_stance_measures_the_same_stride() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let curves = curves();
    let upright = Gait::fitted(&rigging, walk(&curves), &legs, Side::Left).expect("a fitted gait");

    // The same cycle read from a quarter of the way in: the stance now runs
    // across the join rather than sitting at the head of the clip. The keys
    // are evenly spaced and the last repeats the first, so rotating by two
    // of the eight distinct ones is exactly a quarter turn of the cycle.
    let shift = |keys: &[Key; 9]| {
        let mut out = [Key::new(0.0, 0.0); 9];
        for (index, slot) in out.iter_mut().enumerate() {
            *slot = Key::new(
                f64::from(u8::try_from(index).unwrap_or(0)) / 8.0,
                keys[(index + 2) % 8].value,
            );
        }
        out[8] = Key::new(1.0, keys[2].value);
        out
    };
    let (hip, knee) = (shift(&HIP_KEYS), shift(&KNEE_KEYS));
    let shifted = [
        Curve::new(Param::HipSwing(Side::Left), &hip).expect("a real hip curve"),
        Curve::new(Param::KneeBend(Side::Left), &knee).expect("a real knee curve"),
    ];
    let rotated = Gait::fitted(&rigging, walk(&shifted), &legs, Side::Left)
        .expect("a fitted gait across the join");
    assert!(
        mathf::fabs(rotated.stride() - upright.stride()) < 0.1 * upright.stride(),
        "mid-stance {} against head-of-cycle {}",
        rotated.stride(),
        upright.stride()
    );
}

/// A stance that sinks the body while its leg folds into it keeps the foot on
/// the floor but raises it through the body, so where the foot is down is
/// read over the ground: through the body frame the step shows only at its
/// ends, and the stride fitted there is nobody's.
#[test]
fn a_stance_that_sinks_the_body_is_fitted_over_the_ground() {
    const KEYS: usize = 17;
    const CROUCH: f64 = 2.0;
    const DIP: f64 = 4.0;
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let folded = rigging
        .angle_for(Param::KneeBend(Side::Left), 1.0)
        .expect("the humanoid has knees");
    let leg = THIGH_LENGTH + SHANK_LENGTH;

    // The fixture's own step, eight units in front to eight behind and then
    // twelve over, with the body sinking by `DIP` at midstance and the leg
    // solved to keep the foot on the floor meanwhile.
    let mut hip = [Key::new(0.0, 0.0); KEYS];
    let mut knee = hip;
    let mut lift = hip;
    for index in 0..KEYS {
        let phase = f64::from(u8::try_from(index).expect("a small count")) / 16.0;
        let (forward, raised, sunk) = if phase < 0.5 {
            let u = 2.0 * phase;
            let arch = 4.0 * u * (1.0 - u);
            (8.0 * (1.0 - 2.0 * u), 0.0, DIP * arch * arch)
        } else {
            let arch = mathf::sin(core::f64::consts::PI * (2.0 * phase - 1.0));
            let forward = -8.0 * mathf::cos(core::f64::consts::PI * (2.0 * phase - 1.0));
            (forward, 12.0 * arch * arch, 0.0)
        };
        let foot = Body::new(forward, 0.0, CROUCH + sunk + raised - leg);
        let solved = solve(THIGH_LENGTH, SHANK_LENGTH, folded, foot);
        let value = |param, angle| {
            rigging
                .value_for(param, angle)
                .expect("the solve turns each joint the way it travels")
        };
        hip[index] = Key::new(phase, value(Param::HipSwing(Side::Left), solved.pitch));
        knee[index] = Key::new(phase, value(Param::KneeBend(Side::Left), solved.fold));
        lift[index] = Key::new(phase, -(CROUCH + sunk) / leg);
    }
    let curves = [
        Curve::new(Param::HipSwing(Side::Left), &hip).expect("a real hip curve"),
        Curve::new(Param::KneeBend(Side::Left), &knee).expect("a real knee curve"),
    ];
    let sinking = walk(&curves)
        .lifting(Lift::new(&lift).expect("a real lift"))
        .expect("a closing lift");

    let gait = Gait::fitted(&rigging, sinking, &legs, Side::Left).expect("a fitted gait");
    assert!(
        mathf::fabs(gait.stride() - STRIDE) < 0.05 * STRIDE,
        "fitted stride {} should recover {STRIDE}",
        gait.stride()
    );
    let slide = gait
        .slide(&rigging, sinking, &legs, Side::Left)
        .expect("a measured slide");
    assert!(
        slide < 0.01 * STRIDE,
        "a planted foot slid {slide} over a stride of {STRIDE}"
    );
    // The window is the stance: a stride a tenth off carries the body past
    // the planted foot by a tenth of the ground the stance covers, where a
    // window read through the body frame holds only the ends of the step.
    let stance = 0.5;
    let off = Gait::new(gait.stride() * 1.1)
        .expect("a real gait")
        .slide(&rigging, sinking, &legs, Side::Left)
        .expect("a measured slide");
    assert!(
        off > 0.1 * gait.stride() * 0.8 * stance,
        "a stride a tenth off slid only {off}: the window missed most of the stance"
    );
}

/// A restride keeps the phase and changes only how far a cycle carries the
/// figure, so two gaits blended stay in step.
#[test]
fn a_restride_keeps_the_phase_and_changes_the_pace() {
    let mut gait = Gait::new(40.0).expect("a real stride");
    gait.travel(10.0).expect("a real distance");
    let quicker = gait.restrided(20.0).expect("a real stride");
    assert!(mathf::fabs(quicker.phase() - gait.phase()) < 1e-12);
    assert!(mathf::fabs(quicker.stride() - 20.0) < 1e-12);
    let mut quicker = quicker;
    quicker.travel(10.0).expect("a real distance");
    assert!(
        mathf::fabs(quicker.phase() - 0.75) < 1e-12,
        "{}",
        quicker.phase()
    );
    for stride in [0.0, -3.0, f64::NAN, f64::INFINITY] {
        assert_eq!(
            gait.restrided(stride).err(),
            Some(FigureError::StrideUnreal)
        );
    }
}
