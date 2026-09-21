//! What the planting solve puts where, and what it reports when it cannot.

use tairix_util::mathf;

use super::{Leg, Legs};
use crate::error::FigureError;
use crate::frame::Body;
use crate::humanoid::{self, Bone, DRIVES};
use crate::joint::JointId;
use crate::pose::{Param, Pose};
use crate::rig::{Frames, Resolved, Rig};
use crate::rigging::Rigging;
use crate::socket::Side;

/// Figure-local units. The humanoid stands a hundred tall, so a hundredth of
/// a unit is well under a pixel at any scale it is drawn at.
const SLACK: f64 = 1e-6;

fn rig() -> Rig {
    humanoid::rig().expect("the humanoid rig")
}

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
    let rig = rig();
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
    let rig = rig();
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
    let rig = rig();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let pose = Pose::REST;
    let frames = resolved(&rigging, &pose);
    for ground in [[f64::NAN, 0.0], [0.0, f64::INFINITY]] {
        assert_eq!(
            legs.plant(&rigging, &pose, &frames, ground).map(|_| ()),
            Err(FigureError::GroundUnreal)
        );
    }
}

/// The measurements come from the rig rather than from constants beside it,
/// so a taller or wider figure needs no second set of numbers.
#[test]
fn the_reach_and_stance_are_the_rigs_own() {
    let rig = rig();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    // Hips nine either side of centre; a leg spanning a straight forty-seven
    // down to seventeen folded.
    assert!(mathf::fabs(legs.stance() - 18.0) < SLACK);
    assert!(
        legs.reach() > 29.0 && legs.reach() < 31.0,
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
    let rig = rig();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let sole = legs
        .standing(&resolved(&rigging, &Pose::REST))
        .expect("both feet")[0]
        .up;

    for pose in [Pose::REST, striding()] {
        let frames = resolved(&rigging, &pose);
        let planted = legs
            .plant(&rigging, &pose, &frames, [0.0, 0.0])
            .expect("it plants");
        for param in Param::ALL {
            assert!(
                mathf::fabs(planted.pose().get(param) - pose.get(param)) < 1e-6,
                "{param:?} moved from {} to {}",
                pose.get(param),
                planted.pose().get(param)
            );
        }
        assert!(
            planted.worst_miss() < SLACK,
            "missed by {}",
            planted.worst_miss()
        );
        assert_eq!(
            planted.root().basis,
            crate::frame::Basis::IDENTITY,
            "level ground needs no lean"
        );
        let landed = ankle_heights(&rigging, &planted.pose(), planted.root());
        let lowest = mathf::fmin(landed[0], landed[1]);
        assert!(
            mathf::fabs(lowest - sole) < SLACK,
            "the planted foot sits at {lowest}, not on the ground at {sole}"
        );
    }
}

/// The regression the crouch rule exists for: a foot the clip lifted must
/// still be in the air afterwards. Asking every foot for the terrain
/// flattens a walk's swing arc into a shuffle, and the top-down camera looks
/// straight at it.
#[test]
fn a_foot_the_clip_lifted_keeps_its_clearance() {
    let rig = rig();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");

    // The right knee folded well past the left's, which is a leg mid-swing.
    let pose = striding()
        .with(Param::KneeBend(Side::Right), 0.55)
        .expect("a real pose");
    let frames = resolved(&rigging, &pose);
    let before = legs.standing(&frames).expect("both feet");
    let clearance = before[Side::Right as usize].up - before[Side::Left as usize].up;
    assert!(clearance > 1.0, "the fixture must lift a foot at all");

    for ground in [[0.0, 0.0], [2.0, 2.0], [1.5, -1.5], [-3.0, 0.5]] {
        let planted = legs
            .plant(&rigging, &pose, &frames, ground)
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
    let rig = rig();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let pose = Pose::REST;
    let frames = resolved(&rigging, &pose);
    let sole = legs.standing(&frames).expect("both feet")[0].up;

    for ground in [
        [2.0, -2.0],
        [-3.5, 1.5],
        [6.0, 6.0],
        [-4.0, -4.0],
        [0.0, -7.0],
    ] {
        let planted = legs
            .plant(&rigging, &pose, &frames, ground)
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
    let rig = rig();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let pose = Pose::REST;
    let frames = resolved(&rigging, &pose);

    for ground in [[0.0, -5.0], [-5.0, 0.0], [3.0, -2.0], [4.0, 9.0]] {
        let planted = legs
            .plant(&rigging, &pose, &frames, ground)
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
    let rig = rig();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let pose = Pose::REST;
    let frames = resolved(&rigging, &pose);
    let reach = legs.reach();

    let inside = legs
        .plant(&rigging, &pose, &frames, [reach * 0.4, -reach * 0.4])
        .expect("it plants");
    assert_eq!(
        inside.root().basis,
        crate::frame::Basis::IDENTITY,
        "a slope the legs can absorb needs no lean"
    );

    let beyond = legs
        .plant(&rigging, &pose, &frames, [reach, -reach])
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
        .plant(&rigging, &pose, &frames, [-reach, reach])
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
    let rig = rig();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");
    let pose = Pose::REST;
    let frames = resolved(&rigging, &pose);

    // Both feet asked to stand far above the hips they hang from.
    let planted = legs
        .plant(&rigging, &pose, &frames, [400.0, 400.0])
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

/// Whatever the ground, the pose the solve hands back is one the rig admits
/// — the in-limit guarantee survives the planter.
#[test]
fn every_solved_pose_stays_inside_its_parameter_ranges() {
    let rig = rig();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two real legs");

    for pose in [Pose::REST, striding()] {
        let frames = resolved(&rigging, &pose);
        let mut height = -40.0;
        while height <= 40.0 {
            let planted = legs
                .plant(&rigging, &pose, &frames, [height, -height * 0.5])
                .expect("it plants");
            for param in Param::ALL {
                assert!(
                    param.range().holds(planted.pose().get(param)),
                    "{param:?} left its range at height {height}"
                );
            }
            // The pose is admissible, which is the stronger statement: every
            // rotation it becomes is inside its joint's own limit.
            rigging
                .posture(&planted.pose())
                .expect("a solved pose must be posturable");
            height += 1.3;
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
    let rig = rig();
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
        .plant(&kneeling, &pose, &frames, [12.0, 12.0])
        .expect("it still answers");
    assert!(
        planted.worst_miss() > 0.5,
        "a hip that cannot aim must report its shortfall, got {}",
        planted.worst_miss()
    );
    // And the miss must be the honest one: resolving the pose puts the foot
    // exactly where the report says it is.
    let sole = legs.standing(&frames).expect("both feet")[0].up;
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
