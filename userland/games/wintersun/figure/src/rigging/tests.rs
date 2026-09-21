//! What a drive table refuses, and what a posture built from one guarantees.

use tairix_raster::shape::Shape;
use tairix_raster::Color;
use tairix_util::mathf;

use super::{Axis, Drive, Rigging, Sense};
use crate::error::FigureError;
use crate::frame::{Body, Rotation};
use crate::humanoid::{self, Bone};
use crate::joint::{Joint, JointId, Limits};
use crate::pose::{Mask, Param, Pose};
use crate::rig::{Part, Rig};
use crate::socket::Side;

const ROOT: JointId = JointId::new(0);
const CHILD: JointId = JointId::new(1);
const ABSENT: JointId = JointId::new(9);

const TONE: Color = Color::rgb(0x80, 0x80, 0x80);
const SLACK: f64 = 1e-9;

fn close(a: f64, b: f64) -> bool {
    mathf::fabs(a - b) <= SLACK
}

/// A two-joint rig: a mass carrying a limb.
fn fixture() -> Rig {
    let hinge = Limits::hinge(1.0).expect("a radian either way is real");
    Rig::new(
        &[
            Joint::new(None, Body::new(0.0, 0.0, 20.0), hinge),
            Joint::new(Some(ROOT), Body::new(0.0, 0.0, -10.0), hinge),
        ],
        &[
            Part::new(
                ROOT,
                Body::ORIGIN,
                Shape::Superellipse {
                    rx: 8.0,
                    ry: 8.0,
                    square: 0.3,
                },
                TONE,
            ),
            Part::new(
                CHILD,
                Body::ORIGIN,
                Shape::Taper {
                    length: 10.0,
                    top: 3.0,
                    foot: 2.0,
                },
                TONE,
            ),
        ],
        &[],
    )
    .expect("the fixture rig is well formed")
}

fn humanoid_rig() -> Rig {
    humanoid::rig().expect("the shipped humanoid rig is well formed")
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
    let rig = humanoid_rig();
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
    let rig = humanoid_rig();
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
    let rig = humanoid_rig();
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
    let rig = humanoid_rig();
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
    let rig = humanoid_rig();
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
    let rig = humanoid_rig();
    let waist = rig.joints()[Bone::Waist.joint().index()].limits;
    let turned = turn(&rig, Param::SpineBend, 1.0, Bone::Waist);
    assert!(close(turned.pitch, waist.pitch.max()));
}

#[test]
fn a_forward_sense_reaches_the_lower_bound_at_minus_one() {
    let rig = humanoid_rig();
    let waist = rig.joints()[Bone::Waist.joint().index()].limits;
    let turned = turn(&rig, Param::SpineBend, -1.0, Bone::Waist);
    assert!(close(turned.pitch, waist.pitch.min()));
}

#[test]
fn a_reverse_sense_reaches_the_lower_bound_at_one() {
    let rig = humanoid_rig();
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
    let rig = humanoid_rig();
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
    let rig = humanoid_rig();
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
    let rig = humanoid_rig();
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
    let rig = humanoid_rig();
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
    let rig = humanoid_rig();
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
    let rig = humanoid_rig();
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
    let rig = humanoid_rig();
    for side in Side::BOTH {
        let arm = turn(&rig, Param::ShoulderSwing(side), 1.0, Bone::Shoulder(side));
        let leg = turn(&rig, Param::HipSwing(side), 1.0, Bone::Hip(side));
        assert!(arm.pitch < 0.0, "the {side:?} arm swung the wrong way");
        assert!(leg.pitch < 0.0, "the {side:?} leg swung the wrong way");
    }
}

#[test]
fn a_parameter_drives_both_joints_it_names() {
    let rig = humanoid_rig();
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
