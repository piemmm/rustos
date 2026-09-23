//! What a drive table refuses, and what a posture built from one guarantees.

use tairix_raster::Color;
use tairix_util::mathf;

use super::{Axis, Drive, Rigging, Sense};
use crate::error::FigureError;
use crate::frame::{Body, Rotation};
use crate::humanoid::{self, Bone};
use crate::joint::{Joint, JointId, Limits};
use crate::mesh::Ring;
use crate::pose::{Mask, Param, Pose};
use crate::rig::{Part, Rig};
use crate::socket::Side;
use crate::testing::human;
use crate::tint::{Tint, Tints};

const ROOT: JointId = JointId::new(0);
const CHILD: JointId = JointId::new(1);
const ABSENT: JointId = JointId::new(9);

const TONE: Color = Color::rgb(0x80, 0x80, 0x80);
const SLACK: f64 = 1e-9;

fn close(a: f64, b: f64) -> bool {
    mathf::fabs(a - b) <= SLACK
}

/// A mass wide enough to cover the joint below it, and the limb that hangs
/// from it.
const MASS: [Ring; 3] = [
    Ring::new(Body::new(0.0, 0.0, 5.0), 6.0, 6.0),
    Ring::new(Body::ORIGIN, 8.0, 8.0),
    Ring::new(Body::new(0.0, 0.0, -5.0), 6.0, 6.0),
];
const LIMB: [Ring; 2] = [
    Ring::new(Body::ORIGIN, 3.0, 3.0),
    Ring::new(Body::new(0.0, 0.0, -10.0), 2.0, 2.0),
];

/// A two-joint rig: a mass carrying a limb.
fn fixture() -> Rig {
    let hinge = Limits::hinge(1.0).expect("a radian either way is real");
    Rig::new(
        &[
            Joint::new(None, Body::new(0.0, 0.0, 20.0), hinge),
            Joint::new(Some(ROOT), Body::new(0.0, 0.0, -10.0), hinge),
        ],
        &[
            Part::new(ROOT, Body::ORIGIN, &MASS, Tint::Skin).expect("a real mass"),
            Part::new(CHILD, Body::ORIGIN, &LIMB, Tint::Skin).expect("a real limb"),
        ],
        &[],
        Tints::new([TONE; Tint::COUNT]),
    )
    .expect("the fixture rig is well formed")
}

/// How `bone` is turned when `param` is driven to `value`.
fn turn(rig: &Rig, param: Param, value: f64, bone: Bone) -> Rotation {
    let rigging = humanoid::rigging(rig).expect("the shipped table binds");
    let pose = Pose::REST.with(param, value).expect("in range");
    rigging
        .posture(&pose)
        .expect("a pose in range is in limit")
        .get(bone.joint())
        .expect("the rig has the bone")
}

#[test]
fn a_drive_naming_a_joint_the_rig_lacks_is_refused() {
    let rig = fixture();
    let drives = [Drive::new(Param::HeadNod, ABSENT, Axis::Pitch)];
    assert_eq!(
        Rigging::new(&rig, &drives).err(),
        Some(FigureError::NoSuchJoint)
    );
}

#[test]
fn two_drives_on_one_joint_axis_are_refused() {
    let rig = fixture();
    let drives = [
        Drive::new(Param::HeadNod, ROOT, Axis::Pitch),
        Drive::new(Param::SpineBend, ROOT, Axis::Pitch),
    ];
    assert_eq!(
        Rigging::new(&rig, &drives).err(),
        Some(FigureError::DriveCollision)
    );
}

#[test]
fn two_drives_on_different_axes_of_one_joint_are_allowed() {
    let rig = fixture();
    let drives = [
        Drive::new(Param::HeadNod, ROOT, Axis::Pitch),
        Drive::new(Param::HeadTurn, ROOT, Axis::Yaw),
    ];
    assert!(Rigging::new(&rig, &drives).is_ok());
}

#[test]
fn one_parameter_may_drive_several_joints() {
    let rig = fixture();
    let drives = [
        Drive::new(Param::SpineBend, ROOT, Axis::Pitch),
        Drive::new(Param::SpineBend, CHILD, Axis::Pitch),
    ];
    assert!(Rigging::new(&rig, &drives).is_ok());
}

#[test]
fn driven_reports_exactly_the_parameters_in_the_table() {
    let rig = fixture();
    let drives = [
        Drive::new(Param::HeadNod, ROOT, Axis::Pitch),
        Drive::new(Param::HeadTurn, ROOT, Axis::Yaw),
    ];
    let rigging = Rigging::new(&rig, &drives).expect("a sound table");
    assert_eq!(
        rigging.driven(),
        Mask::NONE.with(Param::HeadNod).with(Param::HeadTurn)
    );
}

#[test]
fn the_shipped_humanoid_declares_every_parameter() {
    let rig = human();
    let rigging = humanoid::rigging(&rig).expect("the shipped table binds");
    assert_eq!(rigging.driven(), Mask::ALL);
}

#[test]
fn a_parameter_the_table_does_not_name_turns_nothing() {
    let rig = fixture();
    let drives = [Drive::new(Param::HeadNod, ROOT, Axis::Pitch)];
    let rigging = Rigging::new(&rig, &drives).expect("a sound table");
    assert!(!rigging.driven().holds(Param::HeadTurn));

    let pose = Pose::REST.with(Param::HeadTurn, 1.0).expect("in range");
    let posture = rigging.posture(&pose).expect("in limit");
    for joint in [ROOT, CHILD] {
        let rotation = posture.get(joint).expect("the rig has the joint");
        assert!(close(rotation.pitch, 0.0));
        assert!(close(rotation.yaw, 0.0));
        assert!(close(rotation.roll, 0.0));
    }
}

#[test]
fn rest_turns_nothing() {
    let rig = human();
    let rigging = humanoid::rigging(&rig).expect("the shipped table binds");
    let posture = rigging.posture(&Pose::REST).expect("rest is in limit");
    for bone in Bone::ALL {
        let rotation = posture.get(bone.joint()).expect("the rig has the bone");
        assert!(close(rotation.pitch, 0.0), "{bone:?} pitched at rest");
        assert!(close(rotation.yaw, 0.0), "{bone:?} yawed at rest");
        assert!(close(rotation.roll, 0.0), "{bone:?} rolled at rest");
    }
}

#[test]
fn an_undriven_joint_stays_at_rest_however_the_pose_is_set() {
    let rig = human();
    let rigging = humanoid::rigging(&rig).expect("the shipped table binds");
    let mut pose = Pose::REST;
    for param in Param::ALL {
        pose.set(param, param.range().max()).expect("in range");
    }
    let posture = rigging.posture(&pose).expect("in limit");
    let pelvis = posture
        .get(Bone::Pelvis.joint())
        .expect("the rig has a pelvis");
    assert!(close(pelvis.pitch, 0.0));
    assert!(close(pelvis.yaw, 0.0));
    assert!(close(pelvis.roll, 0.0));
}

#[test]
fn every_extreme_of_every_parameter_stays_inside_the_limits() {
    let rig = human();
    let rigging = humanoid::rigging(&rig).expect("the shipped table binds");

    for param in Param::ALL {
        for value in [param.range().min(), 0.0, param.range().max()] {
            let pose = Pose::REST.with(param, value).expect("in range");
            let posture = rigging
                .posture(&pose)
                .unwrap_or_else(|error| panic!("{param:?} at {value} was refused: {error}"));
            for (index, joint) in rig.joints().iter().enumerate() {
                let id = JointId::new(u8::try_from(index).expect("a rig is small"));
                let rotation = posture.get(id).expect("the rig has the joint");
                assert!(
                    joint.limits.holds(rotation),
                    "{param:?} at {value} put joint {index} outside its limits"
                );
            }
        }
    }
}

#[test]
fn every_parameter_at_its_extreme_at_once_stays_inside_the_limits() {
    let rig = human();
    let rigging = humanoid::rigging(&rig).expect("the shipped table binds");

    for reach in [true, false] {
        let mut pose = Pose::REST;
        for param in Param::ALL {
            let value = if reach {
                param.range().max()
            } else {
                param.range().min()
            };
            pose.set(param, value).expect("in range");
        }
        let posture = rigging.posture(&pose).expect("a pose in range is in limit");
        for (index, joint) in rig.joints().iter().enumerate() {
            let id = JointId::new(u8::try_from(index).expect("a rig is small"));
            let rotation = posture.get(id).expect("the rig has the joint");
            assert!(
                joint.limits.holds(rotation),
                "joint {index} left its limits"
            );
        }
    }
}

#[test]
fn a_forward_sense_reaches_the_upper_bound_at_one() {
    let rig = human();
    let waist = rig.joints()[Bone::Waist.joint().index()].limits;
    let turned = turn(&rig, Param::SpineBend, 1.0, Bone::Waist);
    assert!(close(turned.pitch, waist.pitch.max()));
}

#[test]
fn a_forward_sense_reaches_the_lower_bound_at_minus_one() {
    let rig = human();
    let waist = rig.joints()[Bone::Waist.joint().index()].limits;
    let turned = turn(&rig, Param::SpineBend, -1.0, Bone::Waist);
    assert!(close(turned.pitch, waist.pitch.min()));
}

#[test]
fn a_reverse_sense_reaches_the_lower_bound_at_one() {
    let rig = human();
    let shoulder = rig.joints()[Bone::Shoulder(Side::Left).joint().index()].limits;
    let turned = turn(
        &rig,
        Param::ShoulderSwing(Side::Left),
        1.0,
        Bone::Shoulder(Side::Left),
    );
    assert!(close(turned.pitch, shoulder.pitch.min()));
}

#[test]
fn each_half_of_a_lopsided_limit_is_scaled_on_its_own() {
    let rig = human();
    let shoulder = rig.joints()[Bone::Shoulder(Side::Left).joint().index()].limits;
    let forward = turn(
        &rig,
        Param::ShoulderSwing(Side::Left),
        0.5,
        Bone::Shoulder(Side::Left),
    );
    let back = turn(
        &rig,
        Param::ShoulderSwing(Side::Left),
        -0.5,
        Bone::Shoulder(Side::Left),
    );
    assert!(close(forward.pitch, shoulder.pitch.min() * 0.5));
    assert!(close(back.pitch, shoulder.pitch.max() * 0.5));
}

#[test]
fn an_elbow_cannot_be_driven_past_straight() {
    let rig = human();
    for side in Side::BOTH {
        for step in 0..=20 {
            let value = f64::from(step) / 20.0;
            let turned = turn(&rig, Param::ElbowBend(side), value, Bone::Elbow(side));
            assert!(
                turned.pitch <= SLACK,
                "an elbow bend of {value} opened the joint to {}",
                turned.pitch
            );
        }
    }
}

#[test]
fn a_full_elbow_bend_reaches_the_fold() {
    let rig = human();
    let elbow = rig.joints()[Bone::Elbow(Side::Left).joint().index()].limits;
    let turned = turn(
        &rig,
        Param::ElbowBend(Side::Left),
        1.0,
        Bone::Elbow(Side::Left),
    );
    assert!(close(turned.pitch, elbow.pitch.min()));
}

#[test]
fn a_full_knee_bend_reaches_the_fold() {
    let rig = human();
    let knee = rig.joints()[Bone::Knee(Side::Left).joint().index()].limits;
    let turned = turn(
        &rig,
        Param::KneeBend(Side::Left),
        1.0,
        Bone::Knee(Side::Left),
    );
    assert!(close(turned.pitch, knee.pitch.max()));
}

#[test]
fn an_outward_splay_is_outward_on_both_sides() {
    let rig = human();
    let left = turn(
        &rig,
        Param::ShoulderSplay(Side::Left),
        1.0,
        Bone::Shoulder(Side::Left),
    );
    let right = turn(
        &rig,
        Param::ShoulderSplay(Side::Right),
        1.0,
        Bone::Shoulder(Side::Right),
    );
    assert!(
        left.roll > 0.0,
        "the left arm did not lift away from the body"
    );
    assert!(
        right.roll < 0.0,
        "the right arm did not lift away from the body"
    );
    assert!(close(left.roll, -right.roll), "the sides splayed unequally");
}

#[test]
fn an_outward_hip_splay_is_outward_on_both_sides() {
    let rig = human();
    let left = turn(
        &rig,
        Param::HipSplay(Side::Left),
        1.0,
        Bone::Hip(Side::Left),
    );
    let right = turn(
        &rig,
        Param::HipSplay(Side::Right),
        1.0,
        Bone::Hip(Side::Right),
    );
    assert!(left.roll > 0.0);
    assert!(right.roll < 0.0);
    assert!(close(left.roll, -right.roll));
}

#[test]
fn a_forward_swing_is_forward_on_both_sides() {
    let rig = human();
    for side in Side::BOTH {
        let arm = turn(&rig, Param::ShoulderSwing(side), 1.0, Bone::Shoulder(side));
        let leg = turn(&rig, Param::HipSwing(side), 1.0, Bone::Hip(side));
        assert!(arm.pitch < 0.0, "the {side:?} arm swung the wrong way");
        assert!(leg.pitch < 0.0, "the {side:?} leg swung the wrong way");
    }
}

#[test]
fn a_parameter_drives_both_joints_it_names() {
    let rig = human();
    let neck = turn(&rig, Param::HeadTurn, 1.0, Bone::Neck);
    let skull = turn(&rig, Param::HeadTurn, 1.0, Bone::Head);
    assert!(neck.yaw > 0.0, "the neck did not turn");
    assert!(skull.yaw > 0.0, "the skull did not turn");
}

#[test]
fn a_reversed_drive_is_the_mirror_of_a_forward_one() {
    assert_eq!(
        Drive::new(Param::HeadNod, ROOT, Axis::Pitch).sense,
        Sense::Forward
    );
    assert_eq!(
        Drive::new(Param::HeadNod, ROOT, Axis::Pitch)
            .reversed()
            .sense,
        Sense::Reverse
    );
}

#[test]
fn every_axis_has_its_own_slot() {
    for (slot, axis) in Axis::ALL.iter().enumerate() {
        assert_eq!(axis.index(), slot);
    }
}

/// A parameter may turn several joints, so the angle a layer aiming at
/// something cares about is the whole chain's, not one joint's share.
#[test]
fn an_angle_is_summed_over_every_joint_a_parameter_turns() {
    let rig = human();
    let rigging = humanoid::rigging(&rig).expect("the humanoid rigging");
    let neck = rig.joints()[Bone::Neck.index()].limits.yaw.max();
    let head = rig.joints()[Bone::Head.index()].limits.yaw.max();
    assert!(close(
        rigging
            .angle_for(Param::HeadTurn, 1.0)
            .expect("a driven turn"),
        neck + head
    ));
}

/// The regression this round-trip exists for: a reversed drive swings its
/// limb *forward* on a positive value, so the value that produces a negative
/// angle there is positive. Reading the sign off the angle instead silently
/// answered "this parameter cannot go that way", and an inverse-kinematic
/// layer that asked for a hip angle got nothing back and left the leg where
/// it was.
#[test]
fn a_value_round_trips_through_its_angle_on_every_drive() {
    let rig = human();
    let rigging = humanoid::rigging(&rig).expect("the humanoid rigging");

    for param in Param::ALL {
        let mut value = param.range().min();
        while value <= 1.0 {
            let angle = rigging.angle_for(param, value).expect("a driven parameter");
            let back = rigging
                .value_for(param, angle)
                .expect("an angle its own parameter produced must invert");
            assert!(
                close(back, value),
                "{param:?} at {value} turned {angle} and came back {back}"
            );
            value += 0.125;
        }
    }
}

/// A hip swings forward on a negative pitch, and that is exactly the case
/// the sign bug got wrong.
#[test]
fn a_reversed_drive_inverts_a_negative_angle_to_a_positive_value() {
    let rig = human();
    let rigging = humanoid::rigging(&rig).expect("the humanoid rigging");
    let swing = Param::HipSwing(Side::Left);
    let forward = rigging
        .value_for(swing, -0.4239)
        .expect("a hip must reach a forward swing");
    assert!(forward > 0.0, "a forward swing is a positive value");
    assert!(close(
        rigging.angle_for(swing, forward).expect("a driven hip"),
        -0.4239
    ));
}

/// A joint that folds one way has no value that unfolds it, and an
/// undriven parameter has no value at all.
#[test]
fn an_unreachable_angle_or_an_undriven_parameter_has_no_value() {
    let rig = human();
    let rigging = humanoid::rigging(&rig).expect("the humanoid rigging");
    let knee = Param::KneeBend(Side::Left);
    assert_eq!(rigging.value_for(knee, -0.5), None, "a knee cannot unfold");
    assert_eq!(rigging.value_for(knee, f64::NAN), None);

    let bare = Rigging::new(&rig, &[]).expect("a rigging that drives nothing");
    assert_eq!(bare.angle_for(knee, 1.0), None);
    assert_eq!(bare.value_for(knee, 0.0), None);
}
