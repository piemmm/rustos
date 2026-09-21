//! What a rig refuses, and what placing one guarantees.

use tairix_raster::surface::SUBPIXEL;
use tairix_raster::Color;
use tairix_util::mathf;
use tairix_wintersun_net::value::Facing;

use super::{Fitted, Part, Placement, Posture, Resolved, Rig, Stance, MAX_FITTED};
use crate::error::FigureError;
use crate::frame::{Body, Rotation};
use crate::joint::{Joint, JointId, Limit, Limits};
use crate::mesh::Ring;
use crate::shadow::Light;
use crate::socket::{Mount, Socket};

const ROOT: JointId = JointId::new(0);
const CHILD: JointId = JointId::new(1);
const ABSENT: JointId = JointId::new(9);

/// A stance the placement cases share. What a stance refuses is its own
/// test below, so every other case states only its own subject.
#[track_caller]
fn stance(facing: Facing, scale: f64, at: (f64, f64)) -> Stance {
    Stance::new(facing, scale, at, light()).expect("a real stance")
}

/// A light every placement case shares; what it does to a tone is the mesh
/// module's own test.
#[track_caller]
fn light() -> Light {
    Light::new(-0.6, 0.8, 0.55).expect("a real light")
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
const MASS: [Ring; 3] = [
    Ring::new(Body::new(0.0, 0.0, 5.0), 6.0, 6.0),
    Ring::new(Body::ORIGIN, 8.0, 8.0),
    Ring::new(Body::new(0.0, 0.0, -5.0), 6.0, 6.0),
];

/// A limb hanging the whole way to the joint below it.
const LIMB: [Ring; 2] = [
    Ring::new(Body::ORIGIN, 3.0, 3.0),
    Ring::new(Body::new(0.0, 0.0, -10.0), 2.0, 2.0),
];

#[track_caller]
fn part(joint: JointId, at: Body, rings: &'static [Ring]) -> Part {
    Part::new(joint, at, rings, TONE).expect("a real part")
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
            part(ROOT, Body::ORIGIN, &MASS),
            part(CHILD, Body::ORIGIN, &LIMB),
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
            part(ROOT, Body::new(6.0, 0.0, 0.0), &MASS),
            part(ROOT, Body::new(-6.0, 0.0, 0.0), &MASS),
        ],
        &[],
    )
    .expect("one joint bears nothing, so it needs no mass")
}

/// Where surface `surface` ended up on screen: the mean of every point its
/// strips were walked through.
///
/// A placed surface is a set of strips rather than one anchored outline, so
/// "where it is" is its own centroid — which is what a test asking whether a
/// part moved, turned or scaled actually means.
#[track_caller]
fn centre(out: &Placement, surface: u16) -> (f64, f64) {
    let (mut x, mut y, mut count) = (0i64, 0i64, 0i64);
    for strip in out.strips().filter(|strip| strip.surface == surface) {
        for (px, py) in strip.near.iter().chain(strip.far) {
            x += i64::from(*px);
            y += i64::from(*py);
            count += 1;
        }
    }
    assert!(count > 0, "surface {surface} was not placed");
    let count = f64::from(u32::try_from(count).expect("a small point count"));
    let unit = count * f64::from(SUBPIXEL);
    let real = |sum: i64| f64::from(i32::try_from(sum).expect("inside the canvas"));
    (real(x) / unit, real(y) / unit)
}

/// How far surface `surface` spans on screen, across and down.
#[track_caller]
fn extent(out: &Placement, surface: u16) -> (f64, f64) {
    let (mut lo, mut hi) = ((i32::MAX, i32::MAX), (i32::MIN, i32::MIN));
    for strip in out.strips().filter(|strip| strip.surface == surface) {
        for (px, py) in strip.near.iter().chain(strip.far) {
            lo = (lo.0.min(*px), lo.1.min(*py));
            hi = (hi.0.max(*px), hi.1.max(*py));
        }
    }
    let unit = f64::from(SUBPIXEL);
    (f64::from(hi.0 - lo.0) / unit, f64::from(hi.1 - lo.1) / unit)
}

/// The points surface `surface` was walked through, in strip order.
#[track_caller]
fn points(out: &Placement, surface: u16) -> alloc::vec::Vec<(f64, f64)> {
    let unit = f64::from(SUBPIXEL);
    out.strips()
        .filter(|strip| strip.surface == surface)
        .flat_map(|strip| {
            strip
                .near
                .iter()
                .chain(strip.far)
                .map(|(x, y)| (f64::from(*x) / unit, f64::from(*y) / unit))
                .collect::<alloc::vec::Vec<_>>()
        })
        .collect()
}

/// The order the surfaces were painted in, far-first.
fn order(out: &Placement) -> alloc::vec::Vec<u16> {
    let mut seen: alloc::vec::Vec<u16> = alloc::vec::Vec::new();
    for strip in out.strips() {
        if seen.last() != Some(&strip.surface) {
            seen.push(strip.surface);
        }
    }
    seen
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
                part(ROOT, Body::ORIGIN, &MASS),
                part(ABSENT, Body::ORIGIN, &LIMB),
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
        refuse(&joints(), &[part(CHILD, Body::ORIGIN, &LIMB)], &[],),
        FigureError::BearingJointWithoutMass
    );
}

#[test]
fn a_child_beyond_its_parents_reach_is_refused() {
    // A mass two pixels across cannot cover a joint ten pixels below it, so
    // whatever hangs there is detached however it is posed.
    const PINHEAD: [Ring; 1] = [Ring::new(Body::ORIGIN, 1.0, 1.0)];
    assert_eq!(
        refuse(
            &joints(),
            &[
                part(ROOT, Body::ORIGIN, &PINHEAD),
                part(CHILD, Body::ORIGIN, &LIMB),
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
    const STUB: [Ring; 1] = [Ring::new(Body::ORIGIN, 3.0, 3.0)];
    let rig = Rig::new(
        &joints(),
        &[
            part(ROOT, Body::new(0.0, 0.0, -7.0), &STUB),
            part(CHILD, Body::ORIGIN, &LIMB),
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
                part(ROOT, Body::ORIGIN, &MASS),
                part(CHILD, Body::ORIGIN, &LIMB),
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
                part(ROOT, Body::ORIGIN, &MASS),
                part(CHILD, Body::ORIGIN, &LIMB),
            ],
            &[(Socket::Head, Mount::new(ABSENT, Body::ORIGIN))],
        ),
        FigureError::NoSuchJoint
    );
}

#[test]
fn unreal_geometry_is_refused_wherever_it_is_written() {
    const UNREAL_RING: [Ring; 1] = [Ring::new(Body::ORIGIN, f64::INFINITY, 1.0)];
    let unreal = Body::new(f64::NAN, 0.0, 0.0);
    assert_eq!(
        refuse(&[Joint::new(None, unreal, hinge())], &[], &[]),
        FigureError::GeometryUnreal
    );
    assert_eq!(
        refuse(
            &joints(),
            &[part(ROOT, unreal, &MASS), part(CHILD, Body::ORIGIN, &LIMB),],
            &[],
        ),
        FigureError::GeometryUnreal
    );
    assert_eq!(
        Part::new(ROOT, Body::ORIGIN, &UNREAL_RING, TONE).expect_err("an unreal ring"),
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
    assert_eq!(order(&out).len(), 2);
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
    assert_eq!(order(&out), [1, 0], "the part behind must paint first");
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
    assert_eq!(order(&facing_camera)[0], 1);
    assert_eq!(order(&facing_away)[0], 0);
}

#[test]
fn a_depth_tie_paints_in_the_authored_order() {
    // Two parts at one depth: the rig's own order decides, so a piece of
    // piping stays behind the flap it edges however the figure turns.
    let rig = Rig::new(
        &[Joint::new(None, Body::new(0.0, 0.0, 20.0), hinge())],
        &[
            part(ROOT, Body::ORIGIN, &MASS),
            part(ROOT, Body::ORIGIN, &MASS),
            part(ROOT, Body::ORIGIN, &MASS),
        ],
        &[],
    )
    .expect("consistent");
    let posture = Posture::rest(&rig);
    let mut out = Placement::new();
    posture
        .place(&stance(EAST, 1.0, (0.0, 0.0)), &[], &mut out)
        .expect("places");
    assert_eq!(order(&out), [0, 1, 2]);
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
    let limb = mathf::fabs(centre(&resting, 1).0 - centre(&moved, 1).0);
    let mass = mathf::fabs(centre(&resting, 0).0 - centre(&moved, 0).0);
    assert!(limb > 1.0, "the limb must follow the joint it hangs from");
    // The mass turns in place, because the root's own part is centred on
    // the joint that turned; the limb hangs below it and swings. The mass
    // is not perfectly still because turning a surface turns which half of
    // it faces the camera, and that is what its drawn points follow.
    assert!(
        mass * 4.0 < limb,
        "the mass travelled {mass} against the limb's {limb}"
    );
}

#[test]
fn equipment_rides_the_joint_its_socket_hangs_on() {
    let rig = fixture();
    let blade = Fitted::new(Socket::MainHand, Body::ORIGIN, &MASS, TONE).expect("real gear");
    let mut resting = Placement::new();
    let mut moved = Placement::new();
    Posture::rest(&rig)
        .place(
            &stance(EAST, 1.0, (0.0, 0.0)),
            core::slice::from_ref(&blade),
            &mut resting,
        )
        .expect("places");
    let mut swung = Posture::rest(&rig);
    swung
        .set(CHILD, Rotation::new(0.8, 0.0, 0.0))
        .expect("inside the hinge");
    swung
        .place(
            &stance(EAST, 1.0, (0.0, 0.0)),
            core::slice::from_ref(&blade),
            &mut moved,
        )
        .expect("places");
    assert_eq!(resting.len(), 3, "the rig's parts plus the gear");
    let before = centre(&resting, 2);
    let after = centre(&moved, 2);
    assert!(
        !close(before.0, after.0),
        "gear must swing with the joint its socket is on"
    );
}

#[test]
fn equipment_on_an_unoffered_socket_is_refused() {
    // Refused rather than silently dropped: a helm that vanished would read
    // as a missing asset rather than as a rig that offers no head.
    let rig = fixture();
    let helm = Fitted::new(Socket::Head, Body::ORIGIN, &MASS, TONE).expect("real gear");
    let mut out = Placement::new();
    assert_eq!(
        Posture::rest(&rig).place(
            &stance(EAST, 1.0, (0.0, 0.0)),
            core::slice::from_ref(&helm),
            &mut out
        ),
        Err(FigureError::NoSuchSocket)
    );
}

#[test]
fn unreal_equipment_is_refused() {
    let rig = fixture();
    let bad = Fitted::new(Socket::MainHand, Body::new(0.0, f64::NAN, 0.0), &MASS, TONE)
        .expect("an unreal offset is the placement's refusal, not the gear's");
    let mut out = Placement::new();
    assert_eq!(
        Posture::rest(&rig).place(
            &stance(EAST, 1.0, (0.0, 0.0)),
            core::slice::from_ref(&bad),
            &mut out
        ),
        Err(FigureError::GeometryUnreal)
    );
}

#[test]
fn more_equipment_than_a_figure_carries_is_refused() {
    let rig = fixture();
    let blade = Fitted::new(Socket::MainHand, Body::ORIGIN, &MASS, TONE).expect("real gear");
    let too_much = alloc::vec![blade; MAX_FITTED + 1];
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
            Stance::new(EAST, scale, (0.0, 0.0), light()).map(|_| ()),
            Err(FigureError::ScaleUnreal),
            "scale {scale} must be refused"
        );
    }
    for at in [(f64::NAN, 0.0), (0.0, f64::INFINITY)] {
        assert_eq!(
            Stance::new(EAST, 1.0, at, light()).map(|_| ()),
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

    // Paired by the surface each strip belongs to: the depth order is the
    // paint order and a lift may change it, so walking the two placements
    // side by side would compare different parts.
    for surface in 0..2u16 {
        for (one, other) in points(&flat, surface).iter().zip(points(&lifted, surface)) {
            let grid = 1.0 / f64::from(SUBPIXEL);
            assert!(
                mathf::fabs(other.0 - one.0) <= grid,
                "a lift moves nothing across"
            );
            // Height is unforeshortened, so a lift is exactly its own rows
            // up, to the grid the points are snapped onto.
            assert!(
                mathf::fabs(other.1 - (one.1 - lift)) <= grid,
                "a lift is its own rows up"
            );
        }
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
    for surface in 0..2u16 {
        let (before, after) = (points(&upright, surface), points(&leaning, surface));
        for (one, other) in before.iter().zip(&after) {
            let travel = mathf::hypot(other.0 - one.0, other.1 - one.1);
            // Everything stays within its own radius of the pivot: a
            // rotation about the origin cannot move a point further than
            // twice its distance from it.
            assert!(
                travel <= 2.0 * mathf::hypot(one.0, one.1) + 1e-9,
                "a pivot at the ground point cannot fling a surface outward"
            );
            moved |= travel > 1e-6;
        }
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
    // Every point is snapped to the converter's own sub-pixel grid, so the
    // two agree to that rather than exactly.
    let grid = 1.0 / f64::from(SUBPIXEL);
    let near = |a: f64, b: f64| mathf::fabs(a - b) <= grid;
    for surface in 0..2u16 {
        let (one, other) = (centre(&full, surface), centre(&half, surface));
        assert!(near(other.0, one.0 * 0.5) && near(other.1, one.1 * 0.5));
        let (wide, tall) = extent(&full, surface);
        let (half_wide, half_tall) = extent(&half, surface);
        assert!(near(half_wide, wide * 0.5) && near(half_tall, tall * 0.5));
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
    let grid = 1.0 / f64::from(SUBPIXEL);
    for surface in 0..2u16 {
        let (there, here) = (centre(&shifted, surface), centre(&origin, surface));
        assert!(mathf::fabs(there.0 - (here.0 + 30.0)) <= grid);
        assert!(mathf::fabs(there.1 - (here.1 - 12.0)) <= grid);
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
    for (a, b) in once.strips().zip(twice.strips()) {
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
    let unit = f64::from(SUBPIXEL);
    for strip in out.strips() {
        for (x, y) in strip.near.iter().chain(strip.far) {
            let distance = mathf::hypot(f64::from(*x) / unit, f64::from(*y) / unit);
            assert!(
                distance <= rig.reach() + SLACK,
                "a surface reaches {distance} beyond the stated {}",
                rig.reach()
            );
        }
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
            part(ROOT, Body::ORIGIN, &MASS),
            part(CHILD, Body::ORIGIN, &LIMB),
        ],
        &[],
    )
    .expect("consistent");
    let mut out = Placement::new();
    Posture::rest(&rig)
        .place(&stance(EAST, 1.0, (0.0, 0.0)), &[], &mut out)
        .expect("places");
    let mut square = Placement::new();
    let upright = Rig::new(
        &joints(),
        &[
            part(ROOT, Body::ORIGIN, &MASS),
            part(CHILD, Body::ORIGIN, &LIMB),
        ],
        &[],
    )
    .expect("consistent");
    Posture::rest(&upright)
        .place(&stance(EAST, 1.0, (0.0, 0.0)), &[], &mut square)
        .expect("places");
    assert!(
        !close(centre(&out, 1).0, centre(&square, 1).0),
        "the rest orientation must reach the drawn surface"
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
        &[part(ROOT, Body::ORIGIN, &MASS)],
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

/// The defect the whole mesh pipeline exists to make unspellable: two
/// surfaces that meet at a joint are *rigidly* joined there, whatever the
/// pose and whatever the heading.
///
/// The billboard this replaced could not manage it. It placed a flat outline
/// from an origin, an approximated screen turn and the bone's own
/// unforeshortened length, and all three are wrong off the degenerate
/// headings — measured against the shipped walk, a thigh's drawn end missed
/// its knee by a third of the figure's height. Here the parent's last ring
/// and the child's first are carried by the same joint, so a gap would mean
/// the skinning itself had come apart.
#[test]
fn surfaces_that_meet_at_a_joint_stay_met() {
    use crate::frame::project;
    use crate::humanoid;
    use crate::mesh;
    use crate::pose::{Param, Pose};
    use crate::socket::Side;

    let rig = humanoid::rig().expect("the humanoid rig");
    let rigging = humanoid::rigging(&rig).expect("the humanoid rigging");
    let mut frames = super::Frames::new();

    // Which surfaces meet: a part that spans to a joint, and the part that
    // starts at that joint's own origin. Discovered from the rig rather
    // than listed, so a part added or moved is covered without a second
    // table to keep in step.
    let meeting: alloc::vec::Vec<(usize, usize)> = rig
        .parts()
        .iter()
        .enumerate()
        .filter_map(|(index, part)| {
            let end = part.end?;
            let child = rig.parts().iter().position(|other| {
                other.joint == end
                    && other.end != Some(part.joint)
                    && other
                        .rings
                        .first()
                        .is_some_and(|ring| other.at.plus(ring.at).length() < 1e-12)
            })?;
            Some((index, child))
        })
        .collect();
    assert!(
        meeting.len() >= 6,
        "the humanoid must have limbs that meet at joints, found {}",
        meeting.len()
    );

    let carried = |part: &super::Part, frames: &super::Frames| {
        let own = frames.get(part.joint).expect("a resolved joint");
        let far = part.end.map(|end| {
            let frame = frames.get(end).expect("a resolved joint");
            (frame.at, frame.basis)
        });
        let rest = part.end.map(|end| {
            let joint = rig.joints()[end.index()];
            (joint.at, crate::frame::Basis::of(joint.orientation))
        });
        mesh::carry(part.rings, part.at, (own.at, own.basis), far, rest).expect("it carries")
    };

    // How the two surfaces sit relative to one another in the joint that
    // carries them both. Rigid, so this is the same at every pose — which
    // is the whole claim.
    rigging
        .posture(&Pose::REST)
        .expect("a real posture")
        .resolve(Resolved::REST, &mut frames);
    let seam = |parent: usize, child: usize, frames: &super::Frames| {
        let above = carried(&rig.parts()[parent], frames);
        let below = carried(&rig.parts()[child], frames);
        let (end, start) = (
            above.last().expect("a part has rings").at,
            below.first().expect("a part has rings").at,
        );
        let joint = rig.parts()[parent].end.expect("a spanning part");
        let basis = frames.get(joint).expect("a resolved joint").basis;
        (basis.unapply(end.plus(start.scaled(-1.0))), end, start)
    };
    let rest: alloc::vec::Vec<Body> = meeting
        .iter()
        .map(|(parent, child)| seam(*parent, *child, &frames).0)
        .collect();

    let mut worst = 0.0;
    let mut apart = 0.0;
    for turn in 0..8u16 {
        let facing = Facing(turn * 0x2000);
        for step in 0..6u32 {
            let swing = f64::from(step) / 5.0;
            let mut pose = Pose::REST;
            for side in Side::BOTH {
                pose.set(Param::HipSwing(side), swing * 0.8).expect("real");
                pose.set(Param::KneeBend(side), swing).expect("real");
                pose.set(Param::ShoulderSwing(side), -swing).expect("real");
                pose.set(Param::ElbowBend(side), swing).expect("real");
            }
            rigging
                .posture(&pose)
                .expect("a real posture")
                .resolve(Resolved::REST, &mut frames);

            for (index, (parent, child)) in meeting.iter().enumerate() {
                let (offset, end, start) = seam(*parent, *child, &frames);
                let drift = offset.plus(rest[index].scaled(-1.0)).length();
                worst = mathf::fmax(worst, drift);
                let here = project(facing, end);
                let there = project(facing, start);
                apart = mathf::fmax(apart, mathf::hypot(here.dx - there.dx, here.dy - there.dy));
            }
        }
    }
    // A ten-thousandth of a figure-local unit on a hundred-tall figure:
    // rounding, not motion.
    assert!(worst < 1e-4, "a joined seam drifted {worst} between poses");
    // And they are drawn in contact rather than merely linked: the widest
    // ring either surface carries is under five units, so a separation the
    // size of the figure's own limbs would be the gap the mesh replaced.
    assert!(
        apart < 5.0,
        "two surfaces that meet at a joint drew {apart} apart"
    );
}
