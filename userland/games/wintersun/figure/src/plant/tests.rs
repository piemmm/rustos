//! What the planting solve puts where, and what it reports when it cannot.

use tairix_util::mathf;

use super::{chain, solve, span, Leg, Legs};
use crate::error::FigureError;
use crate::frame::Body;
use crate::humanoid::{self, Bone, DRIVES};
use crate::joint::JointId;
use crate::pose::{Param, Pose};
use crate::rig::{Frames, Resolved};
use crate::rigging::Rigging;
use crate::socket::Side;
use crate::testing::human;

/// Figure-local units. The humanoid stands a hundred tall, so a hundredth of
/// a unit is well under a pixel at any scale it is drawn at.
const SLACK: f64 = 1e-6;

#[track_caller]
fn resolved(rigging: &Rigging<'_>, pose: &Pose) -> Frames {
    let mut frames = Frames::new();
    rigging
        .posture(pose)
        .expect("a real posture")
        .resolve(Resolved::REST, &mut frames);
    frames
}

/// A leg mid-stride, so the solve is tested against a moving figure rather
/// than only against one standing to attention.
fn striding() -> Pose {
    Pose::REST
        .with(Param::HipSwing(Side::Left), 0.21)
        .expect("a real pose")
        .with(Param::KneeBend(Side::Left), 0.20)
        .expect("a real pose")
        .with(Param::HipSwing(Side::Right), -0.14)
        .expect("a real pose")
        .with(Param::KneeBend(Side::Right), 0.11)
        .expect("a real pose")
        .with(Param::AnkleAngle(Side::Left), 0.15)
        .expect("a real pose")
}

#[track_caller]
fn ankle_heights(rigging: &Rigging<'_>, pose: &Pose, root: Resolved) -> [f64; 2] {
    let mut frames = Frames::new();
    rigging
        .posture(pose)
        .expect("a real posture")
        .resolve(root, &mut frames);
    [Side::Left, Side::Right].map(|side| {
        frames
            .get(Bone::Ankle(side).joint())
            .expect("a resolved ankle")
            .at
            .up
    })
}

#[test]
fn a_leg_that_is_not_a_chain_is_refused() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let mut legs = humanoid::legs();
    legs[0].knee = Bone::Elbow(Side::Left).joint();
    assert_eq!(
        Legs::new(&rigging, legs).map(|_| ()),
        Err(FigureError::LegNotAChain)
    );
}

#[test]
fn a_leg_naming_a_joint_the_rig_lacks_is_refused() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    for absent in [0usize, 1, 2] {
        let mut legs = humanoid::legs();
        let leg: &mut Leg = &mut legs[0];
        match absent {
            0 => leg.hip = JointId::new(30),
            1 => leg.knee = JointId::new(30),
            _ => leg.ankle = JointId::new(30),
        }
        assert_eq!(
            Legs::new(&rigging, legs).map(|_| ()),
            Err(FigureError::NoSuchJoint),
            "an absent joint at position {absent} must be refused"
        );
    }
}

#[test]
fn an_unreal_ground_height_is_refused() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let pose = Pose::REST;
    let frames = resolved(&rigging, &pose);
    for ground in [[f64::NAN, 0.0], [0.0, f64::INFINITY]] {
        assert_eq!(
            legs.plant(&rigging, &pose, &frames, ground, 0.0)
                .map(|_| ()),
            Err(FigureError::GroundUnreal)
        );
    }
}

/// A root height beyond the legs' own travel either way is a displacement
/// the simulation authorised, not a cycle's rise and fall, so the solve
/// refuses it rather than drawing a figure nobody placed.
#[test]
fn an_unreal_root_height_is_refused() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let pose = Pose::REST;
    let frames = resolved(&rigging, &pose);
    for root in [f64::NAN, f64::INFINITY, 1.5, -1.5] {
        assert_eq!(
            legs.plant(&rigging, &pose, &frames, [0.0, 0.0], root)
                .map(|_| ()),
            Err(FigureError::LiftOutsideRange),
            "root {root} was admitted"
        );
    }
}

/// The two-bone solve is the one thing both the planter and the shipped
/// clips' authoring check trust, so it is held directly: a point the chain
/// reaches ahead of the hip, behind it, above or below it, and a little to
/// either side as a foot under a hip is, from nearly straight to folded near
/// the knee's limit, is where the angles it answers put the chain's end.
#[test]
fn the_two_bone_solve_puts_the_chain_where_it_was_aimed() {
    let (thigh, shank, folded) = (23.0, 24.0, 2.4);
    let shortest = span(thigh, shank, folded);
    for step in 1..24 {
        let distance = shortest + (thigh + shank - shortest) * f64::from(step) / 24.0;
        for turn in 0..16 {
            let heading = f64::from(turn) * core::f64::consts::TAU / 16.0;
            for lean in [-0.1, 0.0, 0.08] {
                let toward = Body::new(
                    distance * mathf::cos(heading) * mathf::cos(lean),
                    distance * mathf::sin(lean),
                    distance * mathf::sin(heading) * mathf::cos(lean),
                );
                let reached = chain(thigh, shank, solve(thigh, shank, folded, toward));
                let error = reached.plus(toward.scaled(-1.0)).length();
                assert!(
                    error < SLACK,
                    "aimed at {toward:?}, reached {reached:?}, {error} away"
                );
            }
        }
    }
}

/// The height the clip holds the body at raises both ankles by that much of
/// a leg, and a height past a whole leg is refused rather than drawn.
#[test]
fn standing_raises_both_feet_by_the_height_the_clip_holds_the_body_at() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let frames = resolved(&rigging, &striding());
    let rest = legs.standing(&frames, 0.0).expect("both feet");
    for lift in [-1.0, -0.13, 0.4, 1.0] {
        let raised = legs.standing(&frames, lift).expect("both feet");
        for side in [0, 1] {
            // The plan position is untouched, bit for bit.
            assert_eq!(raised[side].forward.to_bits(), rest[side].forward.to_bits());
            assert_eq!(raised[side].side.to_bits(), rest[side].side.to_bits());
            assert!(
                mathf::fabs(raised[side].up - rest[side].up - lift * legs.straight()) < SLACK,
                "lift {lift} raised foot {side} by {}",
                raised[side].up - rest[side].up
            );
        }
    }
    for lift in [f64::NAN, -1.01, 1.5] {
        assert_eq!(
            legs.standing(&frames, lift).map(|_| ()),
            Err(FigureError::LiftOutsideRange),
            "lift {lift} was admitted"
        );
    }
}

/// The measurements come from the rig rather than from constants beside it,
/// so a taller or wider figure needs no second set of numbers.
#[test]
fn the_reach_and_stance_are_the_rigs_own() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let joint = |bone: Bone| rig.joints()[bone.index()].at;
    let stance = joint(Bone::Hip(Side::Left)).side - joint(Bone::Hip(Side::Right)).side;
    let straight = joint(Bone::Knee(Side::Left)).length() + joint(Bone::Ankle(Side::Left)).length();
    assert!(mathf::fabs(legs.stance() - stance) < SLACK);
    assert!(mathf::fabs(legs.straight() - straight) < SLACK);
    // A leg spans from straight down to under two fifths of it folded.
    assert!(
        legs.reach() > 0.6 * straight && legs.reach() < 0.66 * straight,
        "reach {} should be the leg's own span travel",
        legs.reach()
    );
}

/// The property the whole solve rests on: on level ground it must hand back
/// the animation untouched. Without this, every figure on flat ground is
/// quietly redrawn by the planter.
///
/// It holds for *any* pose, not only one whose legs are already straight,
/// because the root absorbs the crouch the clip is standing in — and the
/// foot the clip planted comes out on the floor rather than hovering over
/// it by the depth of that crouch.
#[test]
fn planting_on_flat_ground_changes_nothing() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");

    for pose in [Pose::REST, striding()] {
        let frames = resolved(&rigging, &pose);
        // Whatever height the clip holds the body at: on the level the solve
        // owes the articulation back untouched and the root the clip asked
        // for, so a sheet is a drawing of the clip rather than of the solve.
        for root in [-1.0, -0.2, 0.0, 0.15, 1.0] {
            let planted = legs
                .plant(&rigging, &pose, &frames, [0.0, 0.0], root)
                .expect("it plants");
            for param in Param::ALL {
                assert!(
                    mathf::fabs(planted.pose().get(param) - pose.get(param)) < 1e-6,
                    "root {root}: {param:?} moved from {} to {}",
                    pose.get(param),
                    planted.pose().get(param)
                );
            }
            assert!(
                planted.worst_miss() < SLACK,
                "root {root} missed by {}",
                planted.worst_miss()
            );
            assert_eq!(
                planted.root().basis,
                crate::frame::Basis::IDENTITY,
                "level ground needs no lean"
            );
            let wanted = root * legs.straight();
            assert!(
                mathf::fabs(planted.root().at.up - wanted) < SLACK,
                "root {root}: stood at {} wanted {wanted}",
                planted.root().at.up
            );
        }
    }
}

/// The height a clip states is what puts its planted foot on the floor: a
/// pose folded into its own legs lands only when the root sinks by that
/// fold, which is the agreement `quality::grounding` bounds for the shipped
/// set.
#[test]
fn the_height_a_clip_states_lands_its_planted_foot() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");

    for pose in [Pose::REST, striding()] {
        let frames = resolved(&rigging, &pose);
        let standing = legs.standing(&frames, 0.0).expect("both feet");
        let fold = mathf::fmin(standing[0].up, standing[1].up) - legs.sole();
        let planted = legs
            .plant(
                &rigging,
                &pose,
                &frames,
                [0.0, 0.0],
                -fold / legs.straight(),
            )
            .expect("it plants");
        let landed = ankle_heights(&rigging, &planted.pose(), planted.root());
        let lowest = mathf::fmin(landed[0], landed[1]);
        assert!(
            mathf::fabs(lowest - legs.sole()) < SLACK,
            "the planted foot sits at {lowest}, not on the ground at {}",
            legs.sole()
        );
    }
}

/// The FG4 defect this root model exists to fix. Both legs tucked is a run's
/// flight phase and a deep crouch at once, so a solve that reads the height
/// off the *lesser* fold sinks the figure by that tuck at the moment it
/// should be at its highest — measured on the shipped run at a hundredth of
/// the figure's height, in the wrong direction. A clip that says it is
/// airborne must come out airborne.
#[test]
fn a_tucked_pose_rises_with_its_clip_rather_than_sinking_by_its_fold() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");

    // Both knees well folded and both hips up: a figure with neither foot
    // down, which is what a flight phase is.
    let flight = Pose::REST
        .with(Param::KneeBend(Side::Left), 0.50)
        .expect("a real pose")
        .with(Param::KneeBend(Side::Right), 0.45)
        .expect("a real pose");
    let frames = resolved(&rigging, &flight);
    let standing = legs.standing(&frames, 0.0).expect("both feet");
    let tuck = mathf::fmin(standing[0].up, standing[1].up) - legs.sole();
    assert!(tuck > 1.0, "the fixture must tuck both feet up at all");

    let rise = 0.04;
    let planted = legs
        .plant(&rigging, &flight, &frames, [0.0, 0.0], rise)
        .expect("it plants");
    let stood = planted.root().at.up;
    assert!(
        stood > 0.0,
        "a figure the clip lifted sank to {stood} instead of rising"
    );
    assert!(
        mathf::fabs(stood - rise * legs.straight()) < SLACK,
        "rose to {stood} rather than the {} it asked for",
        rise * legs.straight()
    );
    // The defect's own signature: the old rule would have put the root at
    // minus the lesser tuck, which is below the ground it started from.
    assert!(
        stood > -tuck,
        "the root fell to {stood}, at or below the {} the fold alone gives",
        -tuck
    );
}

/// The regression the crouch rule exists for: a foot the clip lifted must
/// still be in the air afterwards. Asking every foot for the terrain
/// flattens a walk's swing arc into a shuffle, and the top-down camera looks
/// straight at it.
#[test]
fn a_foot_the_clip_lifted_keeps_its_clearance() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");

    // The right knee folded well past the left's, which is a leg mid-swing.
    let pose = striding()
        .with(Param::KneeBend(Side::Right), 0.55)
        .expect("a real pose");
    let frames = resolved(&rigging, &pose);
    let before = legs.standing(&frames, 0.0).expect("both feet");
    let clearance = before[Side::Right as usize].up - before[Side::Left as usize].up;
    assert!(clearance > 1.0, "the fixture must lift a foot at all");

    for ground in [[0.0, 0.0], [2.0, 2.0], [1.5, -1.5], [-3.0, 0.5]] {
        let planted = legs
            .plant(&rigging, &pose, &frames, ground, 0.0)
            .expect("it plants");
        let landed = ankle_heights(&rigging, &planted.pose(), planted.root());
        let kept = landed[Side::Right as usize] - landed[Side::Left as usize];
        assert!(
            mathf::fabs(kept - (clearance + (ground[1] - ground[0]))) < 1e-4,
            "ground {ground:?}: the swing foot kept {kept} of {clearance}"
        );
        assert!(
            planted.worst_miss() < 1e-4,
            "ground {ground:?} missed by {}",
            planted.worst_miss()
        );
    }
}

/// Each foot on its own ground, which is the requirement the top-down camera
/// makes unmissable.
#[test]
fn each_foot_lands_on_its_own_terrain_height() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let pose = Pose::REST;
    let frames = resolved(&rigging, &pose);
    let sole = legs.standing(&frames, 0.0).expect("both feet")[0].up;

    for ground in [
        [2.0, -2.0],
        [-3.5, 1.5],
        [6.0, 6.0],
        [-4.0, -4.0],
        [0.0, -7.0],
    ] {
        let planted = legs
            .plant(&rigging, &pose, &frames, ground, 0.0)
            .expect("it plants");
        assert!(
            planted.worst_miss() < 1e-4,
            "ground {ground:?} missed by {}",
            planted.worst_miss()
        );
        let landed = ankle_heights(&rigging, &planted.pose(), planted.root());
        for side in [Side::Left, Side::Right] {
            assert!(
                mathf::fabs(planted.miss(side)) < 1e-4,
                "ground {ground:?}: {side:?} reported a miss of {}",
                planted.miss(side)
            );
            let wanted = ground[side as usize] + sole;
            assert!(
                mathf::fabs(landed[side as usize] - wanted) < 1e-4,
                "ground {ground:?}: {side:?} foot at {} wanted {wanted}",
                landed[side as usize]
            );
        }
    }
}

/// The root drops to the lowest foot, because that leg is the one that
/// cannot stretch — and it never rises, because a figure does not levitate
/// to meet a hill.
#[test]
fn the_root_drops_to_the_lower_foot_and_never_lifts() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let pose = Pose::REST;
    let frames = resolved(&rigging, &pose);

    for ground in [[0.0, -5.0], [-5.0, 0.0], [3.0, -2.0], [4.0, 9.0]] {
        let planted = legs
            .plant(&rigging, &pose, &frames, ground, 0.0)
            .expect("it plants");
        let lowest = mathf::fmin(mathf::fmin(ground[0], ground[1]), 0.0);
        assert!(
            mathf::fabs(planted.root().at.up - lowest) < SLACK,
            "ground {ground:?} dropped {} wanted {lowest}",
            planted.root().at.up
        );
        assert!(planted.root().at.up <= SLACK, "a figure must not levitate");
    }
}

/// Inside the reach the figure stands square; past it, it leans into the
/// hill rather than tearing the rig apart.
#[test]
fn a_slope_past_the_reach_tilts_the_figure_instead_of_tearing_it() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let pose = Pose::REST;
    let frames = resolved(&rigging, &pose);
    let reach = legs.reach();

    let inside = legs
        .plant(&rigging, &pose, &frames, [reach * 0.4, -reach * 0.4], 0.0)
        .expect("it plants");
    assert_eq!(
        inside.root().basis,
        crate::frame::Basis::IDENTITY,
        "a slope the legs can absorb needs no lean"
    );

    let beyond = legs
        .plant(&rigging, &pose, &frames, [reach, -reach], 0.0)
        .expect("it plants");
    assert_ne!(
        beyond.root().basis,
        crate::frame::Basis::IDENTITY,
        "a slope past the reach must lean"
    );
    // Leaning raises the higher foot, so the left side of the frame goes up.
    assert!(
        beyond.root().basis.apply(Body::SIDE).up > 0.0,
        "the lean must raise the foot that is higher"
    );

    let mirrored = legs
        .plant(&rigging, &pose, &frames, [-reach, reach], 0.0)
        .expect("it plants");
    assert!(
        mirrored.root().basis.apply(Body::SIDE).up < 0.0,
        "the lean must be handed by which foot is higher"
    );
}

/// A place no figure could stand is reported, never fudged: the miss is what
/// tells the simulation it put someone on a cliff.
#[test]
fn ground_no_leg_can_reach_is_reported_as_a_miss() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let pose = Pose::REST;
    let frames = resolved(&rigging, &pose);

    // Both feet asked to stand far above the hips they hang from.
    let planted = legs
        .plant(&rigging, &pose, &frames, [400.0, 400.0], 0.0)
        .expect("it still answers");
    assert!(
        planted.worst_miss() > 1.0,
        "an unreachable foot must report its miss, got {}",
        planted.worst_miss()
    );
    for param in Param::ALL {
        assert!(
            param.range().holds(planted.pose().get(param)),
            "{param:?} left its range at {}",
            planted.pose().get(param)
        );
    }
}

/// Whatever the ground and whatever height the clip holds the body at, the
/// pose the solve hands back is one the rig admits — the in-limit guarantee
/// survives the planter.
#[test]
fn every_solved_pose_stays_inside_its_parameter_ranges() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");

    for pose in [Pose::REST, striding()] {
        let frames = resolved(&rigging, &pose);
        for root in [-1.0, -0.4, 0.0, 0.3, 1.0] {
            let mut height = -40.0;
            while height <= 40.0 {
                let planted = legs
                    .plant(&rigging, &pose, &frames, [height, -height * 0.5], root)
                    .expect("it plants");
                for param in Param::ALL {
                    assert!(
                        param.range().holds(planted.pose().get(param)),
                        "{param:?} left its range at height {height}, root {root}"
                    );
                }
                // The pose is admissible, which is the stronger statement:
                // every rotation it becomes is inside its joint's own limit.
                rigging
                    .posture(&planted.pose())
                    .expect("a solved pose must be posturable");
                height += 1.3;
            }
        }
    }
}

/// The regression this guards: measuring the miss against the angles the
/// solve *asked for* rather than the ones the pose actually holds reports a
/// foot as landed whenever the joint meant to put it there never turned. A
/// rig that folds its knees but cannot swing its hips is exactly that case,
/// and it must report the shortfall rather than a clean landing.
#[test]
fn a_foot_a_rig_cannot_aim_reports_its_miss_rather_than_a_landing() {
    let rig = human();
    let mut kept = [DRIVES[0]; DRIVES.len()];
    let mut count = 0;
    for drive in DRIVES {
        let aims = matches!(drive.param, Param::HipSwing(_) | Param::HipSplay(_));
        if !aims {
            kept[count] = drive;
            count += 1;
        }
    }
    let kneeling = Rigging::new(&rig, &kept[..count]).expect("a rigging with no hips");
    let legs = Legs::new(&kneeling, humanoid::legs()).expect("two real legs");
    let pose = Pose::REST;
    let frames = resolved(&kneeling, &pose);

    // A foot asked well forward of where a knee alone can put it.
    let planted = legs
        .plant(&kneeling, &pose, &frames, [12.0, 12.0], 0.0)
        .expect("it still answers");
    assert!(
        planted.worst_miss() > 0.5,
        "a hip that cannot aim must report its shortfall, got {}",
        planted.worst_miss()
    );
    // And the miss must be the honest one: resolving the pose puts the foot
    // exactly where the report says it is.
    let sole = legs.standing(&frames, 0.0).expect("both feet")[0].up;
    let landed = ankle_heights(&kneeling, &planted.pose(), planted.root());
    for side in [Side::Left, Side::Right] {
        let wanted = 12.0 + sole;
        assert!(
            mathf::fabs((landed[side as usize] - wanted) - planted.miss(side)) < 1e-6,
            "{side:?} landed {} wanted {wanted}, reported {}",
            landed[side as usize],
            planted.miss(side)
        );
    }
}
