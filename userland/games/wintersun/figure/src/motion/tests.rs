//! What the shipped motions are, and what they measure.

use tairix_util::mathf;

use super::{opposite, Kind, Motion};
use crate::clip::{Key, Loop};
use crate::gait::Gait;
use crate::humanoid::{self, Bone, DRIVES};
use crate::pose::Param;
use crate::rigging::Rigging;
use crate::socket::Side;

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
    let rig = humanoid::rig().expect("the humanoid rig");
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
    let rig = humanoid::rig().expect("the humanoid rig");
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let motion = Motion::new(Kind::Idle).expect("a shipped motion");
    let clip = motion.clip().expect("its clip");
    assert_eq!(Kind::Idle.stride(), None);
    assert!(Gait::fitted(&rigging, clip, Bone::Ankle(Side::Left).joint()).is_err());
}
