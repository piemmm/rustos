//! Where a look-at ends up pointing, and what stops it.

use tairix_util::mathf;

use super::Look;
use crate::error::FigureError;
use crate::frame::Body;
use crate::humanoid::{Bone, DRIVES};
use crate::joint::JointId;
use crate::pose::{Param, Pose};
use crate::rig::{Frames, Resolved};
use crate::rigging::Rigging;
use crate::testing::human;

const HEAD: JointId = Bone::Head.joint();

#[track_caller]
fn resolved(rigging: &Rigging<'_>, pose: &Pose) -> Frames {
    let mut frames = Frames::new();
    rigging
        .posture(pose)
        .expect("a real posture")
        .resolve(Resolved::REST, &mut frames);
    frames
}

/// The angle between where the head points and where the target is.
#[track_caller]
fn off_target(rigging: &Rigging<'_>, pose: &Pose, target: Body) -> f64 {
    let frames = resolved(rigging, pose);
    let frame = frames.get(HEAD).expect("a resolved head");
    let toward = target.plus(frame.at.scaled(-1.0));
    let length = toward.length();
    let aim = frame.basis.apply(Body::FORWARD);
    mathf::acos(mathf::clamp(aim.dot(toward) / length, -1.0, 1.0))
}

#[test]
fn an_unreal_share_or_target_is_refused() {
    for share in [-0.1, 1.1, f64::NAN] {
        assert_eq!(
            Look::new(share).map(|_| ()),
            Err(FigureError::ParamOutsideRange),
            "a share of {share} must be refused"
        );
    }
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let frames = resolved(&rigging, &Pose::REST);
    let look = Look::new(0.3).expect("a real look");
    assert_eq!(
        look.at(&rigging, &frames, HEAD, Body::new(f64::NAN, 0.0, 0.0))
            .map(|_| ()),
        Err(FigureError::GeometryUnreal)
    );
    assert_eq!(
        look.at(
            &rigging,
            &frames,
            JointId::new(30),
            Body::new(50.0, 0.0, 80.0)
        )
        .map(|_| ()),
        Err(FigureError::NoSuchJoint)
    );
}

/// A target the head is already pointing at asks for no turn at all.
#[test]
fn a_target_straight_ahead_moves_nothing() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let frames = resolved(&rigging, &Pose::REST);
    let head = frames.get(HEAD).expect("a resolved head").at;
    let ahead = head.plus(Body::new(40.0, 0.0, 0.0));
    let overlay = Look::new(0.25)
        .expect("a real look")
        .at(&rigging, &frames, HEAD, ahead)
        .expect("it looks");
    for param in Param::ALL {
        assert!(
            mathf::fabs(overlay.get(param)) < 1e-9,
            "{param:?} moved by {}",
            overlay.get(param)
        );
    }
}

/// A target sitting on the head itself has no direction to aim at, so the
/// aim is left alone rather than snapping to whatever rounding produced.
#[test]
fn a_target_on_the_head_leaves_the_aim_alone() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let frames = resolved(&rigging, &Pose::REST);
    let head = frames.get(HEAD).expect("a resolved head").at;
    let overlay = Look::new(0.25)
        .expect("a real look")
        .at(&rigging, &frames, HEAD, head)
        .expect("it looks");
    assert!(overlay.is_empty());
}

/// The layer's job: whatever the target, the head finishes nearer to it.
#[test]
fn looking_turns_the_head_toward_the_target() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let look = Look::new(0.35).expect("a real look");

    for target in [
        Body::new(40.0, 30.0, 70.0),
        Body::new(40.0, -30.0, 90.0),
        Body::new(10.0, 50.0, 60.0),
        Body::new(-40.0, 10.0, 80.0),
        Body::new(30.0, 0.0, 20.0),
    ] {
        let frames = resolved(&rigging, &Pose::REST);
        let turned = look
            .at(&rigging, &frames, HEAD, target)
            .expect("it looks")
            .applied(&Pose::REST)
            .expect("it applies");
        let before = off_target(&rigging, &Pose::REST, target);
        let after = off_target(&rigging, &turned, target);
        assert!(
            after < before - 1e-6,
            "target {target:?}: {before} to {after}"
        );
    }
}

/// A target inside the neck's travel is looked *at*, not merely toward.
#[test]
fn a_reachable_target_is_looked_straight_at() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let look = Look::new(0.0).expect("a real look");
    let frames = resolved(&rigging, &Pose::REST);
    let head = frames.get(HEAD).expect("a resolved head").at;
    let target = head.plus(Body::new(40.0, 12.0, -6.0));

    let turned = look
        .at(&rigging, &frames, HEAD, target)
        .expect("it looks")
        .applied(&Pose::REST)
        .expect("it applies");
    assert!(
        off_target(&rigging, &turned, target) < 0.05,
        "off by {}",
        off_target(&rigging, &turned, target)
    );
}

/// A target behind the shoulder asks for more turn than a neck has, so the
/// figure turns as far as it can and no further — it never unscrews its own
/// head.
#[test]
fn a_target_out_of_reach_stops_at_the_limit() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let frames = resolved(&rigging, &Pose::REST);
    let behind = Body::new(-60.0, 20.0, 80.0);

    let turned = Look::new(0.4)
        .expect("a real look")
        .at(&rigging, &frames, HEAD, behind)
        .expect("it looks")
        .applied(&Pose::REST)
        .expect("it applies");
    for param in Param::ALL {
        assert!(
            param.range().holds(turned.get(param)),
            "{param:?} left its range at {}",
            turned.get(param)
        );
    }
    rigging
        .posture(&turned)
        .expect("a looked pose must be posturable");
    assert!(
        off_target(&rigging, &turned, behind) < off_target(&rigging, &Pose::REST, behind),
        "it must still try"
    );
}

/// The share decides how the figure looks doing it, not where it ends up
/// looking: the spine and the neck always sum to the same turn.
#[test]
fn the_spine_share_moves_work_without_moving_the_answer() {
    let rig = human();
    let rigging = Rigging::new(&rig, &DRIVES).expect("the humanoid rigging");
    let frames = resolved(&rigging, &Pose::REST);
    let head = frames.get(HEAD).expect("a resolved head").at;
    let target = head.plus(Body::new(30.0, 14.0, 0.0));

    let mut last = None;
    for share in [0.0, 0.25, 0.5, 0.75, 1.0] {
        let overlay = Look::new(share)
            .expect("a real look")
            .at(&rigging, &frames, HEAD, target)
            .expect("it looks");
        let twist = rigging
            .angle_for(Param::SpineTwist, overlay.get(Param::SpineTwist))
            .expect("a driven twist");
        let turn = rigging
            .angle_for(Param::HeadTurn, overlay.get(Param::HeadTurn))
            .expect("a driven turn");
        let total = twist + turn;
        if let Some(before) = last {
            assert!(
                mathf::fabs(total - before) < 1e-9,
                "share {share} changed the total turn from {before} to {total}"
            );
        }
        last = Some(total);
        if share > 0.0 {
            assert!(twist.abs() > 0.0, "share {share} must use the spine");
        }
    }
}
