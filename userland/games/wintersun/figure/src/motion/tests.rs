//! What the shipped motions are, and what they measure.

use tairix_util::mathf;

use super::action::{
    DODGE, DODGE_ANKLE, DODGE_HIP, DODGE_KNEE, HEAVY_ANKLE, HEAVY_HIP, HEAVY_KNEE, STAGGER,
    STAGGER_ANKLE_LEFT, STAGGER_ANKLE_RIGHT, STAGGER_CATCH, STAGGER_HIP_LEFT, STAGGER_HIP_RIGHT,
    STAGGER_KNEE_LEFT, STAGGER_KNEE_RIGHT,
};
use super::locomotion::{
    IDLE_ANKLE, IDLE_CROUCH, IDLE_HIP, IDLE_KNEE, RUN_ANKLE_LEFT, RUN_CROUCH, RUN_FLIGHT_RISE,
    RUN_HALF_STEP, RUN_HIP_LEFT, RUN_KNEE_LEFT, RUN_STANCE, RUN_STANCE_DIP, WALK_ANKLE_LEFT,
    WALK_CROUCH, WALK_HALF_STEP, WALK_HIP_LEFT, WALK_KNEE_LEFT, WALK_STANCE,
};
use super::state::{
    DIE_ANKLE, DIE_HIP, DIE_KNEE, SIT_ANKLE, SIT_DEPTH, SIT_HIP, SIT_KNEE, SIT_SPLAY,
};
use super::{opposite, rooted, smooth, Kind, Layer, Motion, Set, Sink, Support, LEG_LENGTH};
use crate::clip::{Clip, Key, Loop};
use crate::frame::Body;
use crate::gait::Gait;
use crate::humanoid::{self, Bone, DRIVES, SHANK_LENGTH, THIGH_LENGTH, UPPER_BODY};
use crate::plant::{solve, Legs};
use crate::pose::Param;
use crate::reference;
use crate::rig::{Frames, Resolved};
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
        let floor = |at: f64| floor(clip, at);
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

/// A foot on the floor is a leg's length below the hip, less however far the
/// clip holds the body into its legs.
fn floor(clip: Clip<'_>, phase: f64) -> f64 {
    -LEG_LENGTH * (1.0 + clip.root_at(phase))
}

/// How far a squatting heel lets the ankle turn it: the heel lifts as the
/// squat deepens rather than pinning the ankle at its limit.
const DIE_LEVEL: f64 = 0.6;

/// How much a crouched dodge or chop levels its feet by.
const CROUCHED_LEVEL: f64 = 0.8;

/// How much the stagger's stepping foot is levelled by: it hangs through its
/// swing, as a walking foot does.
const STEPPING_LEVEL: f64 = 0.7;

/// How far behind the hip the stagger's catching foot comes down.
const STAGGER_STEP: f64 = -12.0;

/// When the stagger's catching foot steps back in, and how high each of its
/// two steps clears the floor.
const STAGGER_RETURN: (f64, f64) = (0.70, 0.85);
const STAGGER_CLEARANCE: (f64, f64) = (5.0, 4.0);

/// How far in front of its hip, and how far out from it, a seated foot rests.
const SIT_FOOT: (f64, f64) = (32.0, 3.0);

/// Where the stagger's right foot is against its hip at `phase`: under it,
/// stepping back to catch the body, down behind it, and in again.
fn stagger_step(floor: f64, phase: f64) -> Body {
    let arc = |u: f64, from: f64, to: f64, clearance: f64| {
        let lifted = mathf::sin(core::f64::consts::PI * u);
        Body::new(
            from + (to - from) * smooth(u),
            0.0,
            floor + clearance * lifted * lifted,
        )
    };
    let (back, home) = STAGGER_RETURN;
    if phase <= STAGGER.active {
        Body::new(0.0, 0.0, floor)
    } else if phase <= STAGGER_CATCH {
        let u = (phase - STAGGER.active) / (STAGGER_CATCH - STAGGER.active);
        arc(u, 0.0, STAGGER_STEP, STAGGER_CLEARANCE.0)
    } else if phase <= back {
        Body::new(STAGGER_STEP, 0.0, floor)
    } else if phase <= home {
        arc(
            (phase - back) / (home - back),
            STAGGER_STEP,
            0.0,
            STAGGER_CLEARANCE.1,
        )
    } else {
        Body::new(0.0, 0.0, floor)
    }
}

/// Where `kind`'s `side` foot was stated to be at `phase`, and how much of the
/// leg's turn its ankle levels it by — or `None` where the foot is in the air
/// and its keys are authored rather than solved.
fn footing(kind: Kind, side: Side, clip: Clip<'_>, phase: f64) -> Option<(Body, f64)> {
    let floor = floor(clip, phase);
    let under = Body::new(0.0, 0.0, floor);
    match kind {
        Kind::Idle => Some((IDLE_PATH.foot(clip, phase), IDLE_PATH.level)),
        Kind::Walk => Some((WALK_PATH.foot(clip, phase), WALK_PATH.level)),
        Kind::Run => Some((RUN_PATH.foot(clip, phase), RUN_PATH.level)),
        Kind::Die => Some((under, DIE_LEVEL)),
        Kind::MeleeHeavy => Some((under, CROUCHED_LEVEL)),
        Kind::Dodge => {
            (phase <= DODGE.active || phase >= DODGE.recovery).then_some((under, CROUCHED_LEVEL))
        }
        Kind::Stagger => Some(match side {
            Side::Left => (under, 1.0),
            Side::Right => (stagger_step(floor, phase), STEPPING_LEVEL),
        }),
        Kind::Sit => {
            let outward = match side {
                Side::Left => SIT_FOOT.1,
                Side::Right => -SIT_FOOT.1,
            };
            Some((Body::new(SIT_FOOT.0, outward, floor), 1.0))
        }
        _ => None,
    }
}

/// One solved leg of a shipped clip: its hip, knee and ankle tables, and its
/// splay's where the foot is not under the hip.
struct Solved {
    kind: Kind,
    side: Side,
    hip: &'static [Key],
    knee: &'static [Key],
    ankle: &'static [Key],
    splay: Option<&'static [Key]>,
}

const fn solved(
    kind: Kind,
    side: Side,
    [hip, knee, ankle]: [&'static [Key]; 3],
    splay: Option<&'static [Key]>,
) -> Solved {
    Solved {
        kind,
        side,
        hip,
        knee,
        ankle,
        splay,
    }
}

/// Every leg table solved from a foot path. A side a clip mirrors — the
/// locomotion cycles' right legs — is held by the half-turn test instead.
const SOLVED: [Solved; 11] = [
    solved(
        Kind::Idle,
        Side::Left,
        [&IDLE_HIP, &IDLE_KNEE, &IDLE_ANKLE],
        None,
    ),
    solved(
        Kind::Walk,
        Side::Left,
        [&WALK_HIP_LEFT, &WALK_KNEE_LEFT, &WALK_ANKLE_LEFT],
        None,
    ),
    solved(
        Kind::Run,
        Side::Left,
        [&RUN_HIP_LEFT, &RUN_KNEE_LEFT, &RUN_ANKLE_LEFT],
        None,
    ),
    solved(
        Kind::Dodge,
        Side::Left,
        [&DODGE_HIP, &DODGE_KNEE, &DODGE_ANKLE],
        None,
    ),
    solved(
        Kind::MeleeHeavy,
        Side::Left,
        [&HEAVY_HIP, &HEAVY_KNEE, &HEAVY_ANKLE],
        None,
    ),
    solved(
        Kind::Stagger,
        Side::Left,
        [&STAGGER_HIP_LEFT, &STAGGER_KNEE_LEFT, &STAGGER_ANKLE_LEFT],
        None,
    ),
    solved(
        Kind::Stagger,
        Side::Right,
        [
            &STAGGER_HIP_RIGHT,
            &STAGGER_KNEE_RIGHT,
            &STAGGER_ANKLE_RIGHT,
        ],
        None,
    ),
    solved(
        Kind::Die,
        Side::Left,
        [&DIE_HIP, &DIE_KNEE, &DIE_ANKLE],
        None,
    ),
    solved(
        Kind::Sit,
        Side::Left,
        [&SIT_HIP, &SIT_KNEE, &SIT_ANKLE],
        Some(&SIT_SPLAY),
    ),
    solved(
        Kind::Sit,
        Side::Right,
        [&SIT_HIP, &SIT_KNEE, &SIT_ANKLE],
        Some(&SIT_SPLAY),
    ),
    solved(
        Kind::Idle,
        Side::Right,
        [&IDLE_HIP, &IDLE_KNEE, &IDLE_ANKLE],
        None,
    ),
];

#[test]
fn every_shipped_motion_assembles_and_clips() {
    for kind in Kind::ALL {
        let motion = Motion::new(kind).expect("a shipped motion");
        assert_eq!(motion.kind(), kind);
        let clip = motion.clip().expect("its clip");
        let name = kind.name();
        assert!(clip.seconds() > 0.0, "{name}");
        assert!(!clip.curves().is_empty(), "{name}");
        let cycles = matches!(
            kind,
            Kind::Idle
                | Kind::Walk
                | Kind::Run
                | Kind::Channel
                | Kind::Fall
                | Kind::Sit
                | Kind::Swim
                | Kind::Climb
        );
        let expected = if cycles { Loop::Wrap } else { Loop::Hold };
        assert_eq!(clip.repeat(), expected, "{name}");
        if kind.layer() == Layer::Locomotion {
            assert_eq!(clip.curves().len(), 12, "{name}");
        }
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
/// rather than a description of them: every key of every leg a foot path
/// was solved for is that path put through the planting layer's own two-bone
/// solve, to the six places it is written to. A table edited by hand, or a
/// path whose numbers drift from the keys, fails here.
#[test]
fn every_leg_key_is_its_foot_path_solved() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let folded = rigging
        .angle_for(Param::KneeBend(Side::Left), 1.0)
        .expect("the humanoid has knees");
    for leg in &SOLVED {
        let motion = Motion::new(leg.kind).expect("a shipped motion");
        let clip = motion.clip().expect("its clip");
        let name = leg.kind.name();
        let side = leg.side;
        for index in 0..leg.hip.len() {
            let phase = leg.hip[index].phase;
            let Some((target, level)) = footing(leg.kind, side, clip, phase) else {
                // Only a foot in the air is authored rather than solved.
                assert!(
                    leg.kind == Kind::Dodge && phase > DODGE.active && phase < DODGE.recovery,
                    "{name} {side:?} has no stated path at {phase}"
                );
                continue;
            };
            let solved = solve(THIGH_LENGTH, SHANK_LENGTH, folded, target);
            let turn = solved.pitch + solved.fold;
            let mut wanted = alloc::vec![
                (leg.hip, Param::HipSwing(side), solved.pitch),
                (leg.knee, Param::KneeBend(side), solved.fold),
                (leg.ankle, Param::AnkleAngle(side), -level * turn),
            ];
            match leg.splay {
                Some(splay) => wanted.push((splay, Param::HipSplay(side), solved.roll)),
                None => assert!(
                    mathf::fabs(solved.roll) < 1e-12,
                    "{name} {side:?} splays its hip at {phase}"
                ),
            }
            for (table, param, angle) in wanted {
                let key = table[index];
                let value = rigging
                    .value_for(param, angle)
                    .expect("the solve turns each joint the way it travels");
                assert_eq!(
                    key.phase.to_bits(),
                    phase.to_bits(),
                    "{name} {param:?} keys apart"
                );
                assert!(
                    mathf::fabs(key.value - value) <= ROUNDED,
                    "{name} {param:?} at {phase} is keyed {} but its path solves to {value}",
                    key.value
                );
            }
        }
    }
}

/// An action lasts as long as its three segments, plays once and holds, and
/// fires each of its events on a segment boundary: the frame a hitbox opens
/// or an arrow leaves is the frame the art shows it.
#[test]
fn every_action_is_timed_across_its_segments() {
    let actions = [
        Kind::Dodge,
        Kind::MeleeLight,
        Kind::MeleeHeavy,
        Kind::Draw,
        Kind::Loose,
        Kind::Cast,
        Kind::Hit,
        Kind::Stagger,
    ];
    for kind in Kind::ALL {
        let motion = Motion::new(kind).expect("a shipped motion");
        let clip = motion.clip().expect("its clip");
        let name = kind.name();
        let Some(segments) = clip.segments() else {
            assert!(
                !actions.contains(&kind),
                "{name} is not played as an action"
            );
            continue;
        };
        assert!(actions.contains(&kind), "{name} is played as an action");
        assert_eq!(clip.repeat(), Loop::Hold, "{name}");
        let action = motion
            .authored
            .action
            .expect("an action is authored as one");
        let [windup, active, recovery] = action.seconds;
        assert!(mathf::fabs(clip.seconds() - (windup + active + recovery)) < 1e-12);
        assert_eq!(
            segments.active().to_bits(),
            action.active.to_bits(),
            "{name}"
        );
        assert_eq!(
            segments.recovery().to_bits(),
            action.recovery.to_bits(),
            "{name}"
        );
        // The segment boundaries land where the reference timing puts them.
        let at_active = clip.phase_at(windup).expect("finite");
        let at_recovery = clip.phase_at(windup + active).expect("finite");
        assert!(mathf::fabs(at_active - segments.active()) < 1e-12, "{name}");
        assert!(
            mathf::fabs(at_recovery - segments.recovery()) < 1e-12,
            "{name}"
        );
        for event in clip.events_between(0.0, 1.0).expect("a real range") {
            let on_boundary = [segments.active(), segments.recovery()]
                .iter()
                .any(|phase| phase.to_bits() == event.phase.to_bits());
            let caught = kind == Kind::Stagger && event.phase.to_bits() == STAGGER_CATCH.to_bits();
            assert!(
                on_boundary || caught,
                "{name} fires {} off a segment boundary",
                event.name
            );
        }
    }
}

/// An upper-body clip plays over whatever the legs are doing, so it may key
/// nothing below the waist and carries no height or displacement of its own.
#[test]
fn every_upper_body_clip_keys_only_the_upper_body() {
    for kind in Kind::ALL
        .into_iter()
        .filter(|kind| kind.layer() == Layer::Upper)
    {
        let motion = Motion::new(kind).expect("a shipped motion");
        let clip = motion.clip().expect("its clip");
        for param in Param::ALL {
            if clip.mask().holds(param) {
                assert!(
                    UPPER_BODY.holds(param),
                    "{} keys {param:?}, below the waist",
                    kind.name()
                );
            }
        }
        assert!(clip.travel().is_none(), "{}", kind.name());
        for step in 0..=16 {
            let phase = real(step) / 16.0;
            assert!(mathf::fabs(clip.root_at(phase)) < 1e-12, "{}", kind.name());
        }
    }
}

/// A figure with nothing under it stands at no height of its own: where its
/// body is belongs to whatever is holding it up.
#[test]
fn a_clip_with_nothing_underfoot_holds_no_height_of_its_own() {
    for kind in Kind::ALL
        .into_iter()
        .filter(|kind| kind.support() != Support::Ground)
    {
        let motion = Motion::new(kind).expect("a shipped motion");
        let clip = motion.clip().expect("its clip");
        for step in 0..=16 {
            let phase = real(step) / 16.0;
            assert!(mathf::fabs(clip.root_at(phase)) < 1e-12, "{}", kind.name());
        }
    }
}

/// Each clip that sinks into its legs begins where a standing figure stands
/// and, unless it is a death, ends there too — so it fades in from, and out
/// to, the idle with nothing to cross.
#[test]
fn every_sinking_clip_begins_standing() {
    for sink in [Sink::Dodge, Sink::Heavy, Sink::Stagger, Sink::Die] {
        assert!(
            mathf::fabs(sink.depth(0.0) - IDLE_CROUCH) < 1e-12,
            "{sink:?} starts at {}",
            sink.depth(0.0)
        );
        if sink != Sink::Die {
            assert!(
                mathf::fabs(sink.depth(1.0) - IDLE_CROUCH) < 1e-12,
                "{sink:?} ends at {}",
                sink.depth(1.0)
            );
        }
    }
    // A dead figure stays down.
    assert!(mathf::fabs(Sink::Die.depth(0.8) - Sink::Die.depth(1.0)) < 1e-12);
    let motion = Motion::new(Kind::Sit).expect("the shipped sit");
    let clip = motion.clip().expect("its clip");
    assert!(mathf::fabs(clip.root_at(0.5) - rooted(-SIT_DEPTH)) < 1e-12);
}

/// The dodge lets the simulation move it any distance without a foot
/// sliding: its travel is flat wherever a foot is down, and both feet are
/// off the floor everywhere it is not.
#[test]
fn the_dodge_travels_only_while_both_feet_are_in_the_air() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let motion = Motion::new(Kind::Dodge).expect("the shipped dodge");
    let clip = motion.clip().expect("its clip");
    let travel = clip.travel().expect("a dodge is paced by its clip");
    let mut frames = Frames::new();
    for step in 0..=400 {
        let phase = real(step) / 400.0;
        let spent = travel.at(phase);
        if phase <= DODGE.active {
            assert!(
                mathf::fabs(spent) < 1e-12,
                "moved {spent} before the dash at {phase}"
            );
        } else if phase >= DODGE.recovery {
            assert!(
                mathf::fabs(spent - 1.0) < 1e-12,
                "moved {spent} after the dash at {phase}"
            );
        } else {
            let pose = clip.sample(phase).expect("a pose");
            rigging
                .posture(&pose)
                .expect("posturable")
                .resolve(Resolved::REST, &mut frames);
            let feet = legs
                .standing(&frames, clip.root_at(phase))
                .expect("two feet");
            for foot in feet {
                assert!(
                    foot.up > legs.sole(),
                    "a foot is down at {phase} while the dodge travels"
                );
            }
        }
    }
}

/// The loose begins exactly where the draw holds, so the release follows the
/// draw with nothing to fade across.
#[test]
fn the_loose_begins_where_the_draw_holds() {
    let draw = Motion::new(Kind::Draw).expect("the shipped draw");
    let loose = Motion::new(Kind::Loose).expect("the shipped loose");
    let held = draw.clip().expect("its clip").sample(1.0).expect("a pose");
    let released = loose.clip().expect("its clip").sample(0.0).expect("a pose");
    for param in Param::ALL {
        assert!(
            mathf::fabs(held.get(param) - released.get(param)) < 1e-12,
            "{param:?} jumps from {} to {}",
            held.get(param),
            released.get(param)
        );
    }
}

/// A cycle that does not join hitches every time it comes round, which is
/// the one defect a looping clip can have that a still frame never shows.
#[test]
fn every_looping_curve_closes_on_itself() {
    for kind in Kind::ALL {
        let motion = Motion::new(kind).expect("a shipped motion");
        let clip = motion.clip().expect("its clip");
        if clip.repeat() != Loop::Wrap {
            continue;
        }
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
    for kind in Kind::ALL
        .into_iter()
        .filter(|kind| kind.layer() == Layer::Locomotion)
    {
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
