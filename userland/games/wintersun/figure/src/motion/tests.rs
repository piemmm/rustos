//! What the shipped motions are, and what they measure.

use tairix_util::mathf;

use super::{
    opposite, rooted, Kind, Motion, Set, IDLE_ANKLE, IDLE_CROUCH, IDLE_HIP, IDLE_KNEE, LEG_LENGTH,
    RUN_ANKLE_LEFT, RUN_CROUCH, RUN_FLIGHT_RISE, RUN_HALF_STEP, RUN_HIP_LEFT, RUN_KNEE_LEFT,
    RUN_STANCE, RUN_STANCE_DIP, WALK_ANKLE_LEFT, WALK_CROUCH, WALK_HALF_STEP, WALK_HIP_LEFT,
    WALK_KNEE_LEFT, WALK_STANCE,
};
use crate::clip::{Clip, Key, Loop};
use crate::frame::Body;
use crate::gait::Gait;
use crate::humanoid::{self, Bone, DRIVES, SHANK_LENGTH, THIGH_LENGTH};
use crate::plant::{solve, Legs};
use crate::pose::Param;
use crate::reference;
use crate::rigging::Rigging;
use crate::socket::Side;
use crate::testing::human;

/// A small count as a real, exactly.
fn real(count: usize) -> f64 {
    f64::from(u32::try_from(count).expect("a key count fits a u32"))
}

/// Half a unit in the sixth place the leg tables are written to, and room for
/// the billionth `mathf`'s transcendentals are accurate to.
const ROUNDED: f64 = 5e-7 + 1e-8;

/// A clip's foot path, stated whole: what its leg keys were solved from.
///
/// Down, the foot is on the floor wherever the clip holds the body, and
/// travels back through it at the gait's own rate. Up, it comes forward on
/// the cubic that keeps that rate at both ends, clearing the height it struck
/// at by a sine-squared arch.
struct Path {
    /// How far in front of the hip the foot strikes.
    half_step: f64,
    /// What fraction of the cycle it is down for.
    stance: f64,
    /// How far the swing foot clears the height it struck at.
    clearance: f64,
    /// How much of the leg's own turn the ankle levels the foot by.
    level: f64,
}

/// Under the hip, never lifted, and the foot levelled whole.
const IDLE_PATH: Path = Path {
    half_step: 0.0,
    stance: 1.0,
    clearance: 0.0,
    level: 1.0,
};

const WALK_PATH: Path = Path {
    half_step: WALK_HALF_STEP,
    stance: WALK_STANCE,
    clearance: 7.0,
    level: 0.7,
};

const RUN_PATH: Path = Path {
    half_step: RUN_HALF_STEP,
    stance: RUN_STANCE,
    clearance: 11.0,
    level: 0.5,
};

impl Path {
    /// Where the foot is against the hip at `phase` of `clip`.
    fn foot(&self, clip: Clip<'_>, phase: f64) -> Body {
        // A foot on the floor is a leg's length below the hip, less however
        // far the clip holds the body into its legs.
        let floor = |at: f64| -LEG_LENGTH * (1.0 + clip.root_at(at));
        if phase < self.stance {
            let u = phase / self.stance;
            return Body::new(self.half_step * (1.0 - 2.0 * u), 0.0, floor(phase));
        }
        let u = (phase - self.stance) / (1.0 - self.stance);
        let rate = -(1.0 - self.stance) / self.stance;
        let travelled = u * u * (3.0 - 2.0 * u) + rate * u * (1.0 - u) * (1.0 - 2.0 * u);
        let arch = mathf::sin(core::f64::consts::PI * u);
        Body::new(
            self.half_step * (2.0 * travelled - 1.0),
            0.0,
            floor(0.0) + self.clearance * arch * arch,
        )
    }
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

/// Every table of motions is held in [`Kind::ALL`]'s order, so a kind's index
/// is its place there and the clip a table holds at it is that kind's.
#[test]
fn every_table_of_motions_is_held_in_the_order_kind_lists() {
    let set = Set::new().expect("the shipped set");
    let clips = set.clips().expect("its clips");
    for (place, kind) in Kind::ALL.into_iter().enumerate() {
        assert_eq!(kind.index(), place, "{}", kind.name());
        assert_eq!(
            clips.table()[place],
            set.clip(kind).expect("its clip"),
            "{}",
            kind.name()
        );
        assert_eq!(set.motions[place].kind(), kind);
    }
}

/// The measurement that makes the foot paths the source of the leg tables
/// rather than a description of them: every key of every shipped leg curve
/// is its path put through the planting layer's own two-bone solve, to the
/// six places it is written to. A table edited by hand, or a path whose
/// numbers drift from the keys, fails here.
#[test]
fn every_leg_key_is_its_foot_path_solved() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let folded = rigging
        .angle_for(Param::KneeBend(Side::Left), 1.0)
        .expect("the humanoid has knees");
    let motions: [(Kind, Path, [&[Key]; 3]); 3] = [
        (Kind::Idle, IDLE_PATH, [&IDLE_HIP, &IDLE_KNEE, &IDLE_ANKLE]),
        (
            Kind::Walk,
            WALK_PATH,
            [&WALK_HIP_LEFT, &WALK_KNEE_LEFT, &WALK_ANKLE_LEFT],
        ),
        (
            Kind::Run,
            RUN_PATH,
            [&RUN_HIP_LEFT, &RUN_KNEE_LEFT, &RUN_ANKLE_LEFT],
        ),
    ];
    for (kind, path, tables) in motions {
        let motion = Motion::new(kind).expect("a shipped motion");
        let clip = motion.clip().expect("its clip");
        for index in 0..tables[0].len() {
            let phase = tables[0][index].phase;
            let solved = solve(THIGH_LENGTH, SHANK_LENGTH, folded, path.foot(clip, phase));
            assert!(
                mathf::fabs(solved.roll) < 1e-12,
                "{} splays its hip at {phase}",
                kind.name()
            );
            let turn = solved.pitch + solved.fold;
            let wanted = [
                (Param::HipSwing(Side::Left), solved.pitch),
                (Param::KneeBend(Side::Left), solved.fold),
                (Param::AnkleAngle(Side::Left), -path.level * turn),
            ];
            for (table, (param, angle)) in tables.iter().zip(wanted) {
                let key = table[index];
                let value = rigging
                    .value_for(param, angle)
                    .expect("the solve turns each joint the way it travels");
                assert_eq!(
                    key.phase.to_bits(),
                    phase.to_bits(),
                    "{} {param:?} keys apart",
                    kind.name()
                );
                assert!(
                    mathf::fabs(key.value - value) <= ROUNDED,
                    "{} {param:?} at {phase} is keyed {} but its path solves to {value}",
                    kind.name(),
                    key.value
                );
            }
        }
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
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");

    for kind in [Kind::Walk, Kind::Run] {
        let motion = Motion::new(kind).expect("a shipped motion");
        let clip = motion.clip().expect("its clip");
        let authored = kind.stride().expect("a travelling motion");
        let fitted = Gait::fitted(&rigging, clip, &legs, Side::Left).expect("a fitted gait");
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
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let motion = Motion::new(Kind::Idle).expect("a shipped motion");
    let clip = motion.clip().expect("its clip");
    assert_eq!(Kind::Idle.stride(), None);
    assert!(Gait::fitted(&rigging, clip, &legs, Side::Left).is_err());
}

/// The push-off: the running body sinks from each strike to midstance by the
/// depth its path was solved with and rises again to toe-off, and both steps
/// of the cycle do it alike.
#[test]
fn the_runs_body_sinks_into_each_stance_and_rises_out_of_it() {
    let motion = Motion::new(Kind::Run).expect("a shipped motion");
    let clip = motion.clip().expect("its clip");
    let strike = clip.root_at(0.0);
    assert!(mathf::fabs(strike - rooted(-RUN_CROUCH)) < 1e-12);

    for start in [0.0, 0.5] {
        let midstance = clip.root_at(start + RUN_STANCE / 2.0);
        assert!(
            mathf::fabs(strike - midstance - rooted(RUN_STANCE_DIP)) < 1e-12,
            "the stance from {start} sank {} rather than the authored {}",
            strike - midstance,
            rooted(RUN_STANCE_DIP)
        );
        let mut before = strike;
        for step in 1..=96 {
            let phase = start + RUN_STANCE * real(step) / 96.0;
            let height = clip.root_at(phase);
            let sinking = step <= 48;
            assert!(
                if sinking {
                    height <= before + 1e-12
                } else {
                    height >= before - 1e-12
                },
                "the body turned back at {phase}: {before} to {height}"
            );
            assert!(
                height >= midstance - 1e-12,
                "sank past midstance at {phase}"
            );
            before = height;
        }
        assert!(
            mathf::fabs(before - strike) < 1e-12,
            "toe-off from {start} left at {before}, not the {strike} it struck at"
        );
    }
    for step in 0..=64 {
        let phase = 0.5 * real(step) / 64.0;
        assert!(
            mathf::fabs(clip.root_at(phase) - clip.root_at(phase + 0.5)) < 1e-12,
            "the two steps differ at {phase}"
        );
    }
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

    let toe_off = clip.root_at(RUN_STANCE);
    let apex = clip.root_at(f64::midpoint(RUN_STANCE, 0.5));
    assert!(
        apex > toe_off,
        "mid-flight sits at {apex}, no higher than toe-off's {toe_off}"
    );
    assert!(
        mathf::fabs(apex - toe_off - rooted(RUN_FLIGHT_RISE)) < 1e-12,
        "the rise measured {} rather than the authored {}",
        apex - toe_off,
        rooted(RUN_FLIGHT_RISE)
    );

    // The arc meets the stance height at both ends of each flight window, so
    // the height never steps at the moment a foot takes over.
    for edge in [RUN_STANCE, 0.5, 0.5 + RUN_STANCE, 1.0] {
        assert!(
            mathf::fabs(clip.root_at(edge) - toe_off) < 1e-12,
            "the arc leaves the stance height {toe_off} at phase {edge}"
        );
    }
    // And across each flight it never dips below the height it left the
    // ground at, which is the defect's own signature.
    for start in [RUN_STANCE, 0.5 + RUN_STANCE] {
        for step in 0..=64 {
            let phase = start + (0.5 - RUN_STANCE) * real(step) / 64.0;
            assert!(
                clip.root_at(phase) >= toe_off - 1e-12,
                "the body sank to {} in flight at phase {phase}",
                clip.root_at(phase)
            );
        }
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
