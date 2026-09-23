//! What the shipped motions are, and what they measure.

use tairix_util::mathf;

use super::{
    opposite, rooted, Kind, Motion, IDLE_CROUCH, RUN_FLIGHT_RISE, RUN_STANCE, WALK_CROUCH,
};
use crate::clip::{Key, Loop};
use crate::gait::Gait;
use crate::humanoid::{self, Bone, DRIVES, SHANK_LENGTH, THIGH_LENGTH};
use crate::pose::Param;
use crate::reference;
use crate::rigging::Rigging;
use crate::socket::Side;
use crate::testing::human;

/// A small count as a real, exactly.
fn real(count: usize) -> f64 {
    f64::from(u32::try_from(count).expect("a key count fits a u32"))
}

#[test]
fn every_shipped_motion_assembles_and_clips() {
    for kind in Kind::ALL {
        let motion = Motion::new(kind).expect("a shipped motion");
        assert_eq!(motion.kind(), kind);
        let clip = motion.clip().expect("its clip");
        assert_eq!(clip.repeat(), Loop::Wrap, "{}", kind.name());
        assert!(clip.seconds() > 0.0);
        assert_eq!(clip.curves().len(), 12, "{}", kind.name());
    }
}

/// A cycle that does not join hitches every time it comes round, which is
/// the one defect a looping clip can have that a still frame never shows.
#[test]
fn every_looping_curve_closes_on_itself() {
    for kind in Kind::ALL {
        let motion = Motion::new(kind).expect("a shipped motion");
        let clip = motion.clip().expect("its clip");
        for curve in clip.curves() {
            let keys = curve.keys();
            let (first, last) = (keys[0], keys[keys.len() - 1]);
            assert!(
                mathf::fabs(first.value - last.value) < 1e-12,
                "{} {:?} runs {} to {}",
                kind.name(),
                curve.param(),
                first.value,
                last.value
            );
        }
    }
}

/// Rotating a cycle twice by half of itself is the cycle again — which is
/// only true if the keys are evenly spaced and the last repeats the first,
/// the two properties the mirror depends on.
#[test]
fn a_mirrored_cycle_rotated_twice_is_itself() {
    const SAMPLE: [Key; 5] = [
        Key::new(0.0, 0.1),
        Key::new(0.25, 0.4),
        Key::new(0.5, -0.2),
        Key::new(0.75, 0.9),
        Key::new(1.0, 0.1),
    ];
    for kind in Kind::ALL {
        let motion = Motion::new(kind).expect("a shipped motion");
        let clip = motion.clip().expect("its clip");
        for curve in clip.curves() {
            let keys = curve.keys();
            if keys.len() < 3 {
                continue;
            }
            let spacing = 1.0 / real(keys.len() - 1);
            for (index, key) in keys.iter().enumerate() {
                assert!(
                    mathf::fabs(key.phase - real(index) * spacing) < 1e-9,
                    "{} {:?} key {index} is not evenly spaced",
                    kind.name(),
                    curve.param()
                );
            }
        }
    }
    let there = opposite(&SAMPLE);
    let back = opposite(&there);
    for (a, b) in SAMPLE.iter().zip(back.iter()) {
        assert!(mathf::fabs(a.value - b.value) < 1e-12);
        assert!(mathf::fabs(a.phase - b.phase) < 1e-12);
    }
    // Half a turn on really is half a turn: the mirror at phase zero is the
    // original at phase one half.
    assert!(mathf::fabs(there[0].value - SAMPLE[2].value) < 1e-12);
}

/// The two sides do the same thing half a cycle apart, so a figure cannot
/// walk with a limp nobody animated.
#[test]
fn both_sides_run_the_same_cycle_half_a_turn_apart() {
    for kind in [Kind::Walk, Kind::Run] {
        let motion = Motion::new(kind).expect("a shipped motion");
        let clip = motion.clip().expect("its clip");
        let paired = [
            (Param::HipSwing(Side::Left), Param::HipSwing(Side::Right)),
            (Param::KneeBend(Side::Left), Param::KneeBend(Side::Right)),
            (
                Param::AnkleAngle(Side::Left),
                Param::AnkleAngle(Side::Right),
            ),
            (
                Param::ShoulderSwing(Side::Left),
                Param::ShoulderSwing(Side::Right),
            ),
            (Param::ElbowBend(Side::Left), Param::ElbowBend(Side::Right)),
        ];
        for step in 0..16 {
            let phase = f64::from(step) / 16.0;
            let here = clip.sample(phase).expect("a pose");
            let there = clip.sample((phase + 0.5) % 1.0).expect("a pose");
            for (left, right) in paired {
                assert!(
                    mathf::fabs(here.get(left) - there.get(right)) < 1e-9,
                    "{} {left:?} at {phase} is {} but {right:?} half a turn on is {}",
                    kind.name(),
                    here.get(left),
                    there.get(right)
                );
            }
        }
    }
}

/// The measurement that makes the foot path documentation rather than
/// decoration: the stride fitted out of the keys is the one the path was
/// authored to give.
#[test]
fn the_fitted_stride_recovers_the_authored_foot_path() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let ankle = Bone::Ankle(Side::Left).joint();

    for kind in [Kind::Walk, Kind::Run] {
        let motion = Motion::new(kind).expect("a shipped motion");
        let clip = motion.clip().expect("its clip");
        let authored = kind.stride().expect("a travelling motion");
        let fitted = Gait::fitted(&rigging, clip, ankle).expect("a fitted gait");
        let error = mathf::fabs(fitted.stride() - authored) / authored;
        assert!(
            error < 0.02,
            "{} fitted {} against an authored {authored}",
            kind.name(),
            fitted.stride()
        );
    }
}

/// A motion that stays where it is has no stride to fit, and says so rather
/// than inventing one.
#[test]
fn a_standing_motion_has_no_stride() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let motion = Motion::new(Kind::Idle).expect("a shipped motion");
    let clip = motion.clip().expect("its clip");
    assert_eq!(Kind::Idle.stride(), None);
    assert!(Gait::fitted(&rigging, clip, Bone::Ankle(Side::Left).joint()).is_err());
}

/// The FG4 defect, at the level of the shipped art. A run has a moment with
/// neither foot down, and the height the body is at then is not in its
/// articulation — both legs tucked reads exactly like a deep crouch. Read
/// off the lesser fold, the figure sank by that tuck at the moment it should
/// have been at its highest. Its own curve must lift it instead.
#[test]
fn the_runs_body_rises_while_neither_foot_is_down() {
    let motion = Motion::new(Kind::Run).expect("a shipped motion");
    let clip = motion.clip().expect("its clip");

    let stance = clip.root_at(0.0);
    let apex = clip.root_at(f64::midpoint(RUN_STANCE, 0.5));
    assert!(
        apex > stance,
        "mid-flight sits at {apex}, no higher than the stance's {stance}"
    );
    assert!(
        mathf::fabs(apex - stance - rooted(RUN_FLIGHT_RISE)) < 1e-12,
        "the rise measured {} rather than the authored {}",
        apex - stance,
        rooted(RUN_FLIGHT_RISE)
    );

    // The arc meets the stance height at both ends of each flight window, so
    // the height never steps at the moment a foot takes over.
    for edge in [RUN_STANCE, 0.5, 0.5 + RUN_STANCE, 1.0] {
        assert!(
            mathf::fabs(clip.root_at(edge) - stance) < 1e-12,
            "the arc leaves the stance height {stance} at phase {edge}"
        );
    }
    // And it never dips below the stance height anywhere in the cycle, which
    // is the defect's own signature.
    for step in 0..=256 {
        let phase = real(step) / 256.0;
        assert!(
            clip.root_at(phase) >= stance - 1e-12,
            "the body sank to {} at phase {phase}",
            clip.root_at(phase)
        );
    }
}

/// A walk and an idle keep a foot down at every phase, so their bodies hold
/// one height throughout: there is no moment whose height the articulation
/// could not account for.
#[test]
fn the_grounded_motions_hold_one_height() {
    for (kind, crouch) in [(Kind::Idle, IDLE_CROUCH), (Kind::Walk, WALK_CROUCH)] {
        let motion = Motion::new(kind).expect("a shipped motion");
        let clip = motion.clip().expect("its clip");
        for step in 0..=64 {
            let phase = real(step) / 64.0;
            assert!(
                mathf::fabs(clip.root_at(phase) - rooted(-crouch)) < 1e-12,
                "{} moved its body to {} at phase {phase}",
                kind.name(),
                clip.root_at(phase)
            );
        }
    }
}

/// Every shipped height is a fraction of the figure's own leg, so the curves
/// carry no absolute length of their own and hold on any build: what they
/// need of a rig is the proportion between thigh and shank the foot paths
/// were solved through, and every build keeps it.
#[test]
fn every_shipped_root_height_is_a_fraction_of_a_leg() {
    for figure in &reference::FIGURES {
        let rig = humanoid::rig(&figure.identity().expect("a real record")).expect("builds");
        let thigh = rig.joints()[Bone::Knee(Side::Left).index()].at.length();
        let shank = rig.joints()[Bone::Ankle(Side::Left).index()].at.length();
        assert!(
            mathf::fabs(thigh / shank - THIGH_LENGTH / SHANK_LENGTH) < 1e-12,
            "{}'s leg is {thigh} over {shank}, not the proportion the curves assume",
            figure.name
        );
    }
    for kind in Kind::ALL {
        let motion = Motion::new(kind).expect("a shipped motion");
        let clip = motion.clip().expect("its clip");
        for step in 0..=64 {
            let phase = real(step) / 64.0;
            let height = clip.root_at(phase);
            assert!(
                (-1.0..=1.0).contains(&height),
                "{} asked for {height} of a leg at phase {phase}",
                kind.name()
            );
        }
    }
}
