//! What a rig refuses, and what placing one guarantees.

use tairix_raster::shape::{Placed, Shape};
use tairix_raster::Color;
use tairix_util::mathf;
use tairix_wintersun_net::value::Facing;

use super::{Fitted, Part, Placement, Posture, Resolved, Rig, Stance, MAX_FITTED};
use crate::error::FigureError;
use crate::frame::{Body, Rotation};
use crate::joint::{Joint, JointId, Limit, Limits};
use crate::socket::{Mount, Socket};

const ROOT: JointId = JointId::new(0);
const CHILD: JointId = JointId::new(1);
const ABSENT: JointId = JointId::new(9);

/// A stance the placement cases share. What a stance refuses is its own
/// test below, so every other case states only its own subject.
#[track_caller]
fn stance(facing: Facing, scale: f64, at: (f64, f64)) -> Stance {
    Stance::new(facing, scale, at).expect("a real stance")
}

const EAST: Facing = Facing(0);
const SOUTH: Facing = Facing(0x4000);

const TONE: Color = Color::rgb(0x80, 0x80, 0x80);

const SLACK: f64 = 1e-9;

fn close(a: f64, b: f64) -> bool {
    mathf::fabs(a - b) <= SLACK
}

/// The refusal a rig is expected to produce.
///
/// `Rig` carries no equality — a rig is not a value to compare — so a test
/// asserts on the error rather than on the `Result`.
fn refuse(joints: &[Joint], parts: &[Part], mounts: &[(Socket, Mount)]) -> FigureError {
    Rig::new(joints, parts, mounts).expect_err("this rig must be refused")
}

/// A mass wide enough to cover the child joint below it.
fn mass() -> Shape {
    Shape::Superellipse {
        rx: 8.0,
        ry: 8.0,
        square: 0.3,
    }
}

fn limb() -> Shape {
    Shape::Taper {
        length: 10.0,
        top: 3.0,
        foot: 2.0,
    }
}

fn hinge() -> Limits {
    Limits::hinge(1.0).expect("a radian either way is real")
}

fn joints() -> [Joint; 2] {
    [
        Joint::new(None, Body::new(0.0, 0.0, 20.0), hinge()),
        Joint::new(Some(ROOT), Body::new(0.0, 0.0, -10.0), hinge()),
    ]
}

/// Root carries a mass; the child carries a limb hanging from it.
fn fixture() -> Rig {
    Rig::new(
        &joints(),
        &[
            Part::new(ROOT, Body::ORIGIN, mass(), TONE),
            Part::new(CHILD, Body::ORIGIN, limb(), TONE),
        ],
        &[(
            Socket::MainHand,
            Mount::new(CHILD, Body::new(0.0, 0.0, -10.0)),
        )],
    )
    .expect("the fixture is consistent")
}

/// A rig whose two parts sit fore and aft of the joint, so a depth sort has
/// something to reorder.
fn fore_and_aft() -> Rig {
    Rig::new(
        &[Joint::new(None, Body::new(0.0, 0.0, 20.0), hinge())],
        &[
            Part::new(ROOT, Body::new(6.0, 0.0, 0.0), mass(), TONE),
            Part::new(ROOT, Body::new(-6.0, 0.0, 0.0), mass(), TONE),
        ],
        &[],
    )
    .expect("one joint bears nothing, so it needs no mass")
}

fn placed(out: &Placement, seed: u16) -> Placed {
    out.parts()
        .find(|part| part.seed == seed)
        .expect("every authored part is placed")
}

#[test]
fn a_parent_that_does_not_precede_its_child_is_refused() {
    // Parents first is what makes the hierarchy a forest: without it a cycle
    // could be spelled and the one-pass resolve would read its own output.
    let tangled = [
        Joint::new(Some(CHILD), Body::ORIGIN, hinge()),
        Joint::new(None, Body::ORIGIN, hinge()),
    ];
    assert_eq!(refuse(&tangled, &[], &[]), FigureError::ParentNotEarlier);
}

#[test]
fn a_joint_is_not_its_own_parent() {
    let looped = [Joint::new(Some(ROOT), Body::ORIGIN, hinge())];
    assert_eq!(refuse(&looped, &[], &[]), FigureError::ParentNotEarlier);
}

#[test]
fn a_missing_parent_is_refused() {
    let orphan = [
        Joint::new(None, Body::ORIGIN, hinge()),
        Joint::new(Some(ABSENT), Body::ORIGIN, hinge()),
    ];
    assert_eq!(refuse(&orphan, &[], &[]), FigureError::NoSuchParent);
}

#[test]
fn a_part_on_a_missing_joint_is_refused() {
    assert_eq!(
        refuse(
            &joints(),
            &[
                Part::new(ROOT, Body::ORIGIN, mass(), TONE),
                Part::new(ABSENT, Body::ORIGIN, limb(), TONE),
            ],
            &[],
        ),
        FigureError::NoSuchJoint
    );
}

#[test]
fn a_bearing_joint_that_draws_nothing_is_refused() {
    // The joint-carries-mass rule: without the root's mass the child's limb
    // grows out of thin air, which is the gap at the shoulder this closes.
    assert_eq!(
        refuse(
            &joints(),
            &[Part::new(CHILD, Body::ORIGIN, limb(), TONE)],
            &[],
        ),
        FigureError::BearingJointWithoutMass
    );
}

#[test]
fn a_child_beyond_its_parents_reach_is_refused() {
    // A mass two pixels across cannot cover a joint ten pixels below it, so
    // whatever hangs there is detached however it is posed.
    let pinhead = Shape::Superellipse {
        rx: 1.0,
        ry: 1.0,
        square: 0.0,
    };
    assert_eq!(
        refuse(
            &joints(),
            &[
                Part::new(ROOT, Body::ORIGIN, pinhead, TONE),
                Part::new(CHILD, Body::ORIGIN, limb(), TONE),
            ],
            &[],
        ),
        FigureError::JointBeyondParentReach
    );
}

#[test]
fn a_parents_offset_part_counts_toward_its_reach() {
    // The cover need not be centred on the joint: a mass carried a little
    // below it reaches further down, which is exactly how a haunch works.
    let stub = Shape::Superellipse {
        rx: 3.0,
        ry: 3.0,
        square: 0.0,
    };
    let rig = Rig::new(
        &joints(),
        &[
            Part::new(ROOT, Body::new(0.0, 0.0, -7.0), stub, TONE),
            Part::new(CHILD, Body::ORIGIN, limb(), TONE),
        ],
        &[],
    );
    assert!(rig.is_ok(), "an offset mass still covers its child");
}

#[test]
fn a_second_mount_on_one_socket_is_refused() {
    assert_eq!(
        refuse(
            &joints(),
            &[
                Part::new(ROOT, Body::ORIGIN, mass(), TONE),
                Part::new(CHILD, Body::ORIGIN, limb(), TONE),
            ],
            &[
                (Socket::MainHand, Mount::new(CHILD, Body::ORIGIN)),
                (Socket::MainHand, Mount::new(ROOT, Body::ORIGIN)),
            ],
        ),
        FigureError::DuplicateSocket
    );
}

#[test]
fn a_mount_on_a_missing_joint_is_refused() {
    assert_eq!(
        refuse(
            &joints(),
            &[
                Part::new(ROOT, Body::ORIGIN, mass(), TONE),
                Part::new(CHILD, Body::ORIGIN, limb(), TONE),
            ],
            &[(Socket::Head, Mount::new(ABSENT, Body::ORIGIN))],
        ),
        FigureError::NoSuchJoint
    );
}

#[test]
fn unreal_geometry_is_refused_wherever_it_is_written() {
    let unreal = Body::new(f64::NAN, 0.0, 0.0);
    assert_eq!(
        refuse(&[Joint::new(None, unreal, hinge())], &[], &[]),
        FigureError::GeometryUnreal
    );
    assert_eq!(
        refuse(
            &joints(),
            &[
                Part::new(ROOT, unreal, mass(), TONE),
                Part::new(CHILD, Body::ORIGIN, limb(), TONE),
            ],
            &[],
        ),
        FigureError::GeometryUnreal
    );
    let bad_shape = Shape::Superellipse {
        rx: f64::INFINITY,
        ry: 1.0,
        square: 0.0,
    };
    assert_eq!(
        refuse(
            &joints(),
            &[Part::new(ROOT, Body::ORIGIN, bad_shape, TONE)],
            &[],
        ),
        FigureError::GeometryUnreal
    );
}

#[test]
fn a_rig_reports_what_it_holds() {
    let rig = fixture();
    assert_eq!(rig.joints().len(), 2);
    assert_eq!(rig.parts().len(), 2);
    assert!(rig.mount(Socket::MainHand).is_some());
    assert!(rig.mount(Socket::Head).is_none());
    assert!(rig.reach() > 0.0);
}

#[test]
fn a_posture_refuses_a_rotation_outside_its_limit() {
    // Where a clip that would bend a joint the wrong way fails: here, when
    // it is authored, not on the frame that drew it.
    let rig = fixture();
    let mut posture = Posture::rest(&rig);
    assert_eq!(
        posture.set(CHILD, Rotation::new(1.5, 0.0, 0.0)),
        Err(FigureError::RotationOutsideLimit)
    );
    assert_eq!(
        posture.set(CHILD, Rotation::new(0.0, 0.5, 0.0)),
        Err(FigureError::RotationOutsideLimit),
        "a hinge has no yaw"
    );
    assert_eq!(
        posture.get(CHILD),
        Some(Rotation::REST),
        "a refusal changes nothing"
    );
}

#[test]
fn a_posture_accepts_a_rotation_at_the_limit() {
    let rig = fixture();
    let mut posture = Posture::rest(&rig);
    let stop = Rotation::new(1.0, 0.0, 0.0);
    assert_eq!(posture.set(CHILD, stop), Ok(()));
    assert_eq!(posture.get(CHILD), Some(stop));
}

#[test]
fn a_posture_refuses_a_joint_the_rig_lacks() {
    let rig = fixture();
    let mut posture = Posture::rest(&rig);
    assert_eq!(
        posture.set(ABSENT, Rotation::REST),
        Err(FigureError::NoSuchJoint)
    );
    assert_eq!(posture.get(ABSENT), None);
    assert!(core::ptr::eq(posture.rig(), &raw const rig));
}

#[test]
fn placing_puts_every_part_on_the_surface() {
    let rig = fixture();
    let posture = Posture::rest(&rig);
    let mut out = Placement::new();
    assert!(out.is_empty());
    posture
        .place(&stance(SOUTH, 1.0, (100.0, 200.0)), &[], &mut out)
        .expect("a rest posture places");
    assert_eq!(out.len(), 2);
    assert_eq!(out.parts().len(), 2);
}

#[test]
fn placing_sorts_far_first() {
    // Authored front-first, so a sort that did nothing would leave the near
    // part painting before the far one and the composite inverted.
    let rig = fore_and_aft();
    let posture = Posture::rest(&rig);
    let mut out = Placement::new();
    posture
        .place(&stance(SOUTH, 1.0, (0.0, 0.0)), &[], &mut out)
        .expect("places");
    let order: [u16; 2] = [
        out.parts().next().expect("first").seed,
        out.parts().nth(1).expect("second").seed,
    ];
    assert_eq!(order, [1, 0], "the part behind must paint first");
}

#[test]
fn the_same_arrangement_reverses_when_the_figure_turns_around() {
    // The whole saving: one rig serves every heading, and the turnaround is
    // the sort rather than a second set of parts.
    let rig = fore_and_aft();
    let posture = Posture::rest(&rig);
    let mut facing_camera = Placement::new();
    let mut facing_away = Placement::new();
    posture
        .place(&stance(SOUTH, 1.0, (0.0, 0.0)), &[], &mut facing_camera)
        .expect("places");
    posture
        .place(
            &stance(Facing(0xC000), 1.0, (0.0, 0.0)),
            &[],
            &mut facing_away,
        )
        .expect("places");
    assert_eq!(facing_camera.parts().next().expect("first").seed, 1);
    assert_eq!(facing_away.parts().next().expect("first").seed, 0);
}

#[test]
fn a_depth_tie_paints_in_the_authored_order() {
    // Two parts at one depth: the rig's own order decides, so a piece of
    // piping stays behind the flap it edges however the figure turns.
    let rig = Rig::new(
        &[Joint::new(None, Body::new(0.0, 0.0, 20.0), hinge())],
        &[
            Part::new(ROOT, Body::ORIGIN, mass(), TONE),
            Part::new(ROOT, Body::ORIGIN, mass(), TONE),
            Part::new(ROOT, Body::ORIGIN, mass(), TONE),
        ],
        &[],
    )
    .expect("consistent");
    let posture = Posture::rest(&rig);
    let mut out = Placement::new();
    posture
        .place(&stance(EAST, 1.0, (0.0, 0.0)), &[], &mut out)
        .expect("places");
    let seeds: [u16; 3] = [
        out.parts().next().expect("first").seed,
        out.parts().nth(1).expect("second").seed,
        out.parts().nth(2).expect("third").seed,
    ];
    assert_eq!(seeds, [0, 1, 2]);
}

#[test]
fn a_posed_joint_carries_what_hangs_below_it() {
    // Swinging the root must move the child's limb with it: the child's
    // origin is resolved through its parent's frame, not from the ground.
    let rig = fixture();
    let mut swung = Posture::rest(&rig);
    swung
        .set(ROOT, Rotation::new(0.6, 0.0, 0.0))
        .expect("inside the hinge");
    let mut resting = Placement::new();
    let mut moved = Placement::new();
    Posture::rest(&rig)
        .place(&stance(EAST, 1.0, (0.0, 0.0)), &[], &mut resting)
        .expect("places");
    swung
        .place(&stance(EAST, 1.0, (0.0, 0.0)), &[], &mut moved)
        .expect("places");
    let still = placed(&resting, 1);
    let shifted = placed(&moved, 1);
    assert!(
        !close(still.x, shifted.x),
        "the limb must follow the joint it hangs from"
    );
    assert!(
        !close(still.turn, shifted.turn),
        "and its outline must turn with it"
    );
    // The mass stays put, because the root's own part sits at its origin.
    assert!(close(placed(&resting, 0).x, placed(&moved, 0).x));
}

#[test]
fn equipment_rides_the_joint_its_socket_hangs_on() {
    let rig = fixture();
    let blade = Fitted::new(Socket::MainHand, Body::ORIGIN, mass(), TONE);
    let mut resting = Placement::new();
    let mut moved = Placement::new();
    Posture::rest(&rig)
        .place(&stance(EAST, 1.0, (0.0, 0.0)), &[blade], &mut resting)
        .expect("places");
    let mut swung = Posture::rest(&rig);
    swung
        .set(CHILD, Rotation::new(0.8, 0.0, 0.0))
        .expect("inside the hinge");
    swung
        .place(&stance(EAST, 1.0, (0.0, 0.0)), &[blade], &mut moved)
        .expect("places");
    assert_eq!(resting.len(), 3, "the rig's parts plus the gear");
    let before = placed(&resting, 2);
    let after = placed(&moved, 2);
    assert!(
        !close(before.x, after.x),
        "gear must swing with the joint its socket is on"
    );
}

#[test]
fn equipment_on_an_unoffered_socket_is_refused() {
    // Refused rather than silently dropped: a helm that vanished would read
    // as a missing asset rather than as a rig that offers no head.
    let rig = fixture();
    let helm = Fitted::new(Socket::Head, Body::ORIGIN, mass(), TONE);
    let mut out = Placement::new();
    assert_eq!(
        Posture::rest(&rig).place(&stance(EAST, 1.0, (0.0, 0.0)), &[helm], &mut out),
        Err(FigureError::NoSuchSocket)
    );
}

#[test]
fn unreal_equipment_is_refused() {
    let rig = fixture();
    let bad = Fitted::new(
        Socket::MainHand,
        Body::new(0.0, f64::NAN, 0.0),
        mass(),
        TONE,
    );
    let mut out = Placement::new();
    assert_eq!(
        Posture::rest(&rig).place(&stance(EAST, 1.0, (0.0, 0.0)), &[bad], &mut out),
        Err(FigureError::GeometryUnreal)
    );
}

#[test]
fn more_equipment_than_a_figure_carries_is_refused() {
    let rig = fixture();
    let blade = Fitted::new(Socket::MainHand, Body::ORIGIN, mass(), TONE);
    let too_much = [blade; MAX_FITTED + 1];
    let mut out = Placement::new();
    assert_eq!(
        Posture::rest(&rig).place(&stance(EAST, 1.0, (0.0, 0.0)), &too_much, &mut out),
        Err(FigureError::TooMuchEquipment)
    );
}

#[test]
fn an_unreal_scale_or_anchor_is_refused() {
    for scale in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert_eq!(
            Stance::new(EAST, scale, (0.0, 0.0)).map(|_| ()),
            Err(FigureError::ScaleUnreal),
            "scale {scale} must be refused"
        );
    }
    for at in [(f64::NAN, 0.0), (0.0, f64::INFINITY)] {
        assert_eq!(
            Stance::new(EAST, 1.0, at).map(|_| ()),
            Err(FigureError::GeometryUnreal)
        );
    }
}

#[test]
fn an_unreal_root_is_refused() {
    for offset in [
        Body::new(f64::NAN, 0.0, 0.0),
        Body::new(0.0, 0.0, f64::INFINITY),
    ] {
        assert_eq!(
            Resolved::rooted(offset, Rotation::REST).map(|_| ()),
            Err(FigureError::GeometryUnreal)
        );
    }
    assert_eq!(
        Resolved::rooted(Body::ORIGIN, Rotation::new(f64::NAN, 0.0, 0.0)).map(|_| ()),
        Err(FigureError::GeometryUnreal)
    );
}

/// The root is the frame the parentless joints hang in, so a lift moves every
/// part by exactly it and a tilt turns the figure about its ground contact
/// rather than about any joint.
#[test]
fn the_root_displaces_the_whole_figure_from_its_ground_point() {
    let rig = fixture();
    let posture = Posture::rest(&rig);
    let (mut flat, mut lifted) = (Placement::new(), Placement::new());
    let base = stance(EAST, 1.0, (0.0, 0.0));
    let lift = 7.0;
    posture
        .place(&base, &[], &mut flat)
        .expect("the flat figure places");
    posture
        .place(
            &base.rooted(
                Resolved::rooted(Body::new(0.0, 0.0, lift), Rotation::REST).expect("a real root"),
            ),
            &[],
            &mut lifted,
        )
        .expect("the lifted figure places");

    for (before, after) in flat.parts().zip(lifted.parts()) {
        close(after.x, before.x);
        // Height is unforeshortened, so a lift is exactly its own rows up.
        close(after.y, before.y - lift);
        close(after.turn, before.turn);
    }
}

/// A tilt pivots at the ground point, so the part sitting there does not move
/// while everything above it swings — which is the whole reason the tilt is
/// the placement's and not the pelvis joint's.
#[test]
fn a_root_tilt_turns_the_figure_about_its_ground_contact() {
    let rig = fixture();
    let posture = Posture::rest(&rig);
    let (mut upright, mut leaning) = (Placement::new(), Placement::new());
    let base = stance(EAST, 1.0, (0.0, 0.0));
    posture
        .place(&base, &[], &mut upright)
        .expect("the upright figure places");
    posture
        .place(
            &base.rooted(
                Resolved::rooted(Body::ORIGIN, Rotation::new(0.0, 0.0, 0.40)).expect("a real root"),
            ),
            &[],
            &mut leaning,
        )
        .expect("the leaning figure places");

    let mut moved = false;
    for (before, after) in upright.parts().zip(leaning.parts()) {
        let travel = mathf::hypot(after.x - before.x, after.y - before.y);
        // Everything stays within its own radius of the pivot: a rotation
        // about the origin cannot move a point further than twice its
        // distance from it.
        assert!(
            travel <= 2.0 * mathf::hypot(before.x, before.y) + 1e-9,
            "a pivot at the ground point cannot fling a part outward"
        );
        moved |= travel > 1e-6;
    }
    assert!(moved, "a tilt must actually turn the figure");
}

#[test]
fn scaling_moves_the_offsets_and_the_outline_together() {
    // A figure drawn at half size must be half the figure, not a full-size
    // arrangement of half-size shapes.
    let rig = fixture();
    let posture = Posture::rest(&rig);
    let mut full = Placement::new();
    let mut half = Placement::new();
    posture
        .place(&stance(EAST, 1.0, (0.0, 0.0)), &[], &mut full)
        .expect("places");
    posture
        .place(&stance(EAST, 0.5, (0.0, 0.0)), &[], &mut half)
        .expect("places");
    for seed in 0..2u16 {
        let one = placed(&full, seed);
        let other = placed(&half, seed);
        assert!(close(other.x, one.x * 0.5));
        assert!(close(other.y, one.y * 0.5));
        assert!(close(other.shape.reach(), one.shape.reach() * 0.5));
        assert!(close(other.turn, one.turn), "a scale is not a rotation");
    }
}

#[test]
fn the_anchor_translates_the_whole_figure() {
    let rig = fixture();
    let posture = Posture::rest(&rig);
    let mut origin = Placement::new();
    let mut shifted = Placement::new();
    posture
        .place(&stance(EAST, 1.0, (0.0, 0.0)), &[], &mut origin)
        .expect("places");
    posture
        .place(&stance(EAST, 1.0, (30.0, -12.0)), &[], &mut shifted)
        .expect("places");
    for seed in 0..2u16 {
        assert!(close(
            placed(&shifted, seed).x,
            placed(&origin, seed).x + 30.0
        ));
        assert!(close(
            placed(&shifted, seed).y,
            placed(&origin, seed).y - 12.0
        ));
    }
}

#[test]
fn placing_the_same_posture_twice_gives_the_same_figure() {
    // A frame must not depend on the buffer's history, or a digest over it
    // would differ between an interleaved and a repeated draw.
    let rig = fixture();
    let mut posture = Posture::rest(&rig);
    posture
        .set(CHILD, Rotation::new(0.4, 0.0, 0.0))
        .expect("legal");
    let mut once = Placement::new();
    let mut twice = Placement::new();
    posture
        .place(&stance(SOUTH, 1.5, (7.0, 9.0)), &[], &mut once)
        .expect("places");
    // Reuse a buffer that already holds a different figure.
    Posture::rest(&rig)
        .place(&stance(EAST, 3.0, (0.0, 0.0)), &[], &mut twice)
        .expect("places");
    posture
        .place(&stance(SOUTH, 1.5, (7.0, 9.0)), &[], &mut twice)
        .expect("places");
    assert_eq!(once.len(), twice.len());
    for (a, b) in once.parts().zip(twice.parts()) {
        assert_eq!(a, b);
    }
}

#[test]
fn reach_bounds_every_part_at_rest() {
    // The bound a caller sizes a surface from, held to the parts themselves.
    let rig = fixture();
    let posture = Posture::rest(&rig);
    let mut out = Placement::new();
    posture
        .place(&stance(EAST, 1.0, (0.0, 0.0)), &[], &mut out)
        .expect("places");
    for part in out.parts() {
        let distance = mathf::hypot(part.x, part.y) + part.shape.reach();
        assert!(
            distance <= rig.reach() + SLACK,
            "a part reaches {distance} beyond the stated {}",
            rig.reach()
        );
    }
}

#[test]
fn a_rest_orientation_turns_a_joint_without_a_posture() {
    // A splayed limb is stated once on the joint, so it needs no posture to
    // sit where it rests and no offset baked into its child.
    let splayed = [
        Joint::new(None, Body::new(0.0, 0.0, 20.0), hinge()),
        Joint::new(Some(ROOT), Body::new(0.0, 0.0, -10.0), hinge())
            .oriented(Rotation::new(0.5, 0.0, 0.0)),
    ];
    let rig = Rig::new(
        &splayed,
        &[
            Part::new(ROOT, Body::ORIGIN, mass(), TONE),
            Part::new(CHILD, Body::ORIGIN, limb(), TONE),
        ],
        &[],
    )
    .expect("consistent");
    let mut out = Placement::new();
    Posture::rest(&rig)
        .place(&stance(EAST, 1.0, (0.0, 0.0)), &[], &mut out)
        .expect("places");
    assert!(
        !close(placed(&out, 1).turn, 0.0),
        "the rest orientation must reach the outline"
    );
}

#[test]
fn a_limit_that_fixes_an_axis_admits_only_rest_on_it() {
    let rig = Rig::new(
        &[Joint::new(
            None,
            Body::new(0.0, 0.0, 20.0),
            Limits::new(
                Limit::symmetric(0.5).expect("real"),
                Limit::FIXED,
                Limit::FIXED,
            ),
        )],
        &[Part::new(ROOT, Body::ORIGIN, mass(), TONE)],
        &[],
    )
    .expect("consistent");
    let mut posture = Posture::rest(&rig);
    assert!(posture.set(ROOT, Rotation::new(0.5, 0.0, 0.0)).is_ok());
    assert_eq!(
        posture.set(ROOT, Rotation::new(0.0, 0.0, 0.1)),
        Err(FigureError::RotationOutsideLimit)
    );
}
