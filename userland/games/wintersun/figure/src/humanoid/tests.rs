//! The humanoid's own consistency: its proportions, its hierarchy, and the
//! rule that a joint bearing a limb carries its mass.

use tairix_util::mathf;
use tairix_wintersun_net::value::Facing;

use alloc::vec::Vec;
use tairix_raster::surface::SUBPIXEL;

use super::{
    feature, rig, Bone, BODY_PARTS, FOOT, JOINT_COUNT, LEAST_REACH, MOST_PARTS, MOST_REACH,
    STANDING_HEIGHT,
};
use crate::error::FigureError;
use crate::frame::{Rotation, FORESHORTEN};
use crate::identity::{Build, Features, Identity, Palette, Setting, Spec};
use crate::mesh;
use crate::reference::{self, Reference};
use crate::rig::{Frames, Part, Placement, Posture, Resolved, Rig, Stance};
use crate::species::Species;
use crate::testing::{corners, human};
use crate::tint::{Tint, Tints};

/// Where surface `surface` ended up on screen, as the mean of every point
/// its strips were walked through.
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

/// Whether two placed surfaces overlap on screen.
///
/// Boundary points are sampled at a few angles round each ring, so two
/// surfaces that genuinely touch need not share one: what says they are
/// still joined is that the region each covers meets the other's.
#[track_caller]
fn overlap(out: &Placement, one: u16, other: u16) -> bool {
    let box_of = |surface: u16| {
        let (mut lo, mut hi) = ((i32::MAX, i32::MAX), (i32::MIN, i32::MIN));
        for strip in out.strips().filter(|strip| strip.surface == surface) {
            for (x, y) in strip.near.iter().chain(strip.far) {
                lo = (lo.0.min(*x), lo.1.min(*y));
                hi = (hi.0.max(*x), hi.1.max(*y));
            }
        }
        (lo, hi)
    };
    let (a_lo, a_hi) = box_of(one);
    let (b_lo, b_hi) = box_of(other);
    a_lo.0 <= b_hi.0 && b_lo.0 <= a_hi.0 && a_lo.1 <= b_hi.1 && b_lo.1 <= a_hi.1
}

/// A stance the placement cases share; what a stance refuses is `rig`'s own
/// test, so these state only their own subject.
#[track_caller]
fn stance(facing: Facing, scale: f64, at: (f64, f64)) -> Stance {
    Stance::new(facing, scale, at, Reference::light().expect("a real light"))
        .expect("a real stance")
}
use crate::socket::{Side, Socket};

const EAST: Facing = Facing(0);
const SOUTH: Facing = Facing(0x4000);

const SLACK: f64 = 1e-9;

fn built() -> Rig {
    human()
}

#[test]
fn the_shipped_rig_assembles() {
    // Every check `Rig::new` makes, applied to the rig the game draws — so
    // editing a table into something inconsistent fails here. The reference
    // human is its body, a two-part style of hair, two eyes and two ears.
    let rig = built();
    assert_eq!(rig.joints().len(), JOINT_COUNT);
    assert_eq!(rig.parts().len(), BODY_PARTS + 6);
}

#[test]
fn every_bone_names_its_own_joint() {
    // The named handle a clip poses by, held to the table it indexes.
    let rig = built();
    let mut seen = [false; JOINT_COUNT];
    for bone in Bone::ALL {
        let slot = bone.index();
        assert!(slot < rig.joints().len(), "{bone:?} indexes past the table");
        assert!(!seen[slot], "{bone:?} shares a joint");
        seen[slot] = true;
    }
    assert!(seen.into_iter().all(|used| used), "a joint has no name");
}

#[test]
fn the_bone_list_is_in_table_order() {
    for (position, bone) in Bone::ALL.into_iter().enumerate() {
        assert_eq!(bone.index(), position);
    }
}

#[test]
fn every_chain_hangs_off_the_pelvis() {
    // A forest with one root: a second root would be a figure in two pieces
    // that the placed output would still happily draw.
    let rig = built();
    let roots = rig
        .joints()
        .iter()
        .filter(|joint| joint.parent.is_none())
        .count();
    assert_eq!(roots, 1);
    assert!(rig.joints()[Bone::Pelvis.index()].parent.is_none());
}

#[test]
fn the_hierarchy_runs_where_anatomy_does() {
    let rig = built();
    let parent_of = |bone: Bone| rig.joints()[bone.index()].parent;
    assert_eq!(parent_of(Bone::Chest), Some(Bone::Waist.joint()));
    assert_eq!(parent_of(Bone::Head), Some(Bone::Neck.joint()));
    for side in Side::BOTH {
        assert_eq!(parent_of(Bone::Shoulder(side)), Some(Bone::Chest.joint()));
        assert_eq!(
            parent_of(Bone::Elbow(side)),
            Some(Bone::Shoulder(side).joint())
        );
        assert_eq!(
            parent_of(Bone::Wrist(side)),
            Some(Bone::Elbow(side).joint())
        );
        assert_eq!(parent_of(Bone::Hip(side)), Some(Bone::Pelvis.joint()));
        assert_eq!(parent_of(Bone::Knee(side)), Some(Bone::Hip(side).joint()));
        assert_eq!(parent_of(Bone::Ankle(side)), Some(Bone::Knee(side).joint()));
    }
}

#[test]
fn a_limb_and_its_mass_share_one_joint() {
    // The rule, stated as a count: a shoulder and a hip each carry two parts
    // — the cap the trunk shows and the limb that swings — and both are on
    // that one joint, so no posture can part them.
    let rig = built();
    for side in Side::BOTH {
        for bearing in [Bone::Shoulder(side), Bone::Hip(side)] {
            let carried = rig
                .parts()
                .iter()
                .filter(|part| part.joint() == bearing.joint())
                .count();
            assert_eq!(carried, 2, "{bearing:?} must carry its cap and its limb");
        }
    }
}

#[test]
fn a_swinging_limb_never_leaves_its_mass() {
    // The defect this rule closes, measured: whatever the arm does, the cap
    // that covers its socket is placed at exactly the same point.
    let rig = built();
    let mut posture = Posture::rest(&rig);
    for side in Side::BOTH {
        posture
            .set(Bone::Shoulder(side).joint(), Rotation::new(-1.2, 0.3, 0.0))
            .expect("inside the shoulder's limits");
        posture
            .set(Bone::Hip(side).joint(), Rotation::new(-0.9, 0.0, 0.0))
            .expect("inside the hip's limits");
    }
    let mut out = Placement::new();
    posture
        .place(&stance(SOUTH, 1.0, (0.0, 0.0)), &[], &mut out)
        .expect("places");

    for bearing in [
        Bone::Shoulder(Side::Left),
        Bone::Shoulder(Side::Right),
        Bone::Hip(Side::Left),
        Bone::Hip(Side::Right),
    ] {
        let mut seeds = rig
            .parts()
            .iter()
            .enumerate()
            .filter(|(_, part)| part.joint() == bearing.joint())
            .map(|(index, _)| u16::try_from(index).expect("inside the bound"));
        let cap = seeds.next().expect("the cap");
        let limb = seeds.next().expect("the limb");
        // The two surfaces ride one joint, so wherever the joint went they
        // both went.
        assert!(
            overlap(&out, cap, limb),
            "{bearing:?} parted its mass from its limb"
        );
    }
}

/// How far above the ground the skull's crown and the foot's sole stand for
/// `identity` at rest, measured off the carried rings themselves.
///
/// Not off a part's reach, which is a radial bound and over-states a tall
/// part's height by its own width, and not off the placed rows, whose spread
/// includes the depth between the two feet. The skull and the foot are
/// found by the rings they are built from, so hair, horns and an animal's
/// ears — which are not stature — are not counted.
fn crown_and_sole(identity: &Identity) -> (f64, f64) {
    let rig = rig(identity).expect("every identity builds");
    let skull = feature::skull_for(identity.spec().features.face);
    let mut frames = Frames::new();
    Posture::rest(&rig).resolve(Resolved::REST, &mut frames);
    let (mut crown, mut sole) = (f64::MIN, f64::MAX);
    for part in rig.parts() {
        let is_skull = part.rings() == skull;
        let is_foot = part.rings() == &FOOT[..];
        if !is_skull && !is_foot {
            continue;
        }
        let own = frames.get(part.joint()).expect("a resolved joint");
        let hoops = mesh::carry(
            part.rings(),
            part.stretch(),
            part.at(),
            (own.at, own.basis),
            None,
            None,
        )
        .expect("the part carries");
        for hoop in &hoops {
            // A cross-section is an ellipse in space, so how tall it stands
            // is the vertical reach of its two half-axes together.
            let half = mathf::hypot(hoop.wide.up, hoop.deep.up);
            if is_skull {
                crown = mathf::fmax(crown, hoop.at.up + half);
            } else {
                sole = mathf::fmin(sole, hoop.at.up - half);
            }
        }
    }
    (crown, sole)
}

#[test]
fn the_figure_stands_its_stated_height() {
    // What a record's height setting means, measured independently of the
    // arithmetic that built the skeleton, for every species at every one of
    // its build corners: the crown stands exactly where the setting says,
    // and the sole is on the ground rather than in or above it.
    for species in Species::ALL {
        for identity in corners(species) {
            let (crown, sole) = crown_and_sole(&identity);
            let stated = identity.proportions().height * STANDING_HEIGHT;
            assert!(
                mathf::fabs(sole) < 1e-9,
                "{species:?} stands {sole} off the ground"
            );
            assert!(crown > sole, "{species:?} has no crown above its sole");
            assert!(
                mathf::fabs(crown - sole - stated) < 1e-9,
                "{species:?} stands {} against a stated {stated}",
                crown - sole
            );
        }
    }
}

#[test]
fn limb_and_head_proportion_change_the_shape_and_not_the_height() {
    // A proportion is not a size: a long-limbed figure of one height has the
    // longer leg and the shorter trunk, and a large head costs the body.
    let mut short = reference::spec(Species::Human);
    short.build.limbs = Setting::LOW;
    let mut long = short;
    long.build.limbs = Setting::HIGH;
    let leg = |spec: Spec| {
        let rig = rig(&Identity::new(spec).expect("real")).expect("builds");
        let knee = rig.joints()[Bone::Knee(Side::Left).index()].at.length();
        let ankle = rig.joints()[Bone::Ankle(Side::Left).index()].at.length();
        let chest = rig.joints()[Bone::Chest.index()].at.length();
        (knee + ankle, chest)
    };
    let (short_leg, short_trunk) = leg(short);
    let (long_leg, long_trunk) = leg(long);
    assert!(long_leg > short_leg, "longer limbs are longer legs");
    assert!(
        long_trunk < short_trunk,
        "at one height, longer legs cost the trunk"
    );
    let (short_crown, _) = crown_and_sole(&Identity::new(short).expect("real"));
    let (long_crown, _) = crown_and_sole(&Identity::new(long).expect("real"));
    assert!(
        mathf::fabs(short_crown - long_crown) < 1e-9,
        "the same height"
    );
}

#[test]
fn a_larger_head_carries_everything_on_it_and_the_gear_it_wears() {
    let mut small = reference::spec(Species::Dwarf);
    small.build.head = Setting::LOW;
    let mut large = small;
    large.build.head = Setting::HIGH;
    let head_of = |spec: Spec| {
        let rig = rig(&Identity::new(spec).expect("real")).expect("builds");
        let eye = rig
            .parts()
            .iter()
            .find(|part| part.tint() == Tint::Eyes)
            .expect("an eye")
            .at();
        let helm = rig.mount(Socket::Head).expect("a head socket");
        (eye, helm.scale)
    };
    let (small_eye, small_helm) = head_of(small);
    let (large_eye, large_helm) = head_of(large);
    assert!(
        large_eye.length() > small_eye.length(),
        "the eye rides out with the face"
    );
    assert!(large_helm > small_helm, "a helm on a larger head is larger");
}

#[test]
fn girth_widens_the_body_and_taper_moves_it_between_shoulders_and_hips() {
    let shoulder_and_hip = |spec: Spec| {
        let rig = rig(&Identity::new(spec).expect("real")).expect("builds");
        let shoulder = rig.joints()[Bone::Shoulder(Side::Left).index()].at.side;
        let hip = rig.joints()[Bone::Hip(Side::Left).index()].at.side;
        (shoulder, hip)
    };
    let mut slight = reference::spec(Species::Human);
    slight.build.girth = Setting::LOW;
    let mut heavy = slight;
    heavy.build.girth = Setting::HIGH;
    let (slight_shoulder, slight_hip) = shoulder_and_hip(slight);
    let (heavy_shoulder, heavy_hip) = shoulder_and_hip(heavy);
    assert!(heavy_shoulder > slight_shoulder && heavy_hip > slight_hip);

    let mut pear = reference::spec(Species::Human);
    pear.build.taper = Setting::LOW;
    let mut wedge = pear;
    wedge.build.taper = Setting::HIGH;
    let (pear_shoulder, pear_hip) = shoulder_and_hip(pear);
    let (wedge_shoulder, wedge_hip) = shoulder_and_hip(wedge);
    assert!(
        wedge_shoulder > pear_shoulder,
        "a high taper broadens the shoulders"
    );
    assert!(wedge_hip < pear_hip, "and narrows the hips");
}

#[test]
fn the_foot_further_into_the_scene_draws_higher() {
    // The foreshortening doing its job on a figure rather than on a single
    // offset: facing east the left leg is the far one, so it draws higher up
    // the screen than the right. Were the depth axis not foreshortened the
    // two would sit a whole nine pixels apart instead of six.
    let rig = built();
    let mut out = Placement::new();
    Posture::rest(&rig)
        .place(&stance(EAST, 1.0, (0.0, 0.0)), &[], &mut out)
        .expect("places");

    let row_of = |bone: Bone| {
        let seed = rig
            .parts()
            .iter()
            .position(|part| part.joint() == bone.joint())
            .and_then(|index| u16::try_from(index).ok())
            .expect("the ankle carries a part");
        centre(&out, seed).1
    };

    let far = row_of(Bone::Ankle(Side::Left));
    let near = row_of(Bone::Ankle(Side::Right));
    assert!(far < near, "the far foot must draw higher up the screen");
    let apart = near - far;
    let hips_apart = rig.joints()[Bone::Hip(Side::Left).index()].at.side
        - rig.joints()[Bone::Hip(Side::Right).index()].at.side;
    assert!(
        mathf::fabs(apart - hips_apart * FORESHORTEN) < 0.5,
        "the feet are {apart} apart, not the foreshortened {}",
        hips_apart * FORESHORTEN
    );
}

#[test]
fn the_two_sides_are_mirror_images() {
    // Authored from one table, so a difference here means the mirroring
    // itself broke rather than one limb being edited.
    let rig = built();
    for (left, right) in [
        (Bone::Shoulder(Side::Left), Bone::Shoulder(Side::Right)),
        (Bone::Hip(Side::Left), Bone::Hip(Side::Right)),
    ] {
        let a = rig.joints()[left.index()];
        let b = rig.joints()[right.index()];
        assert!(mathf::fabs(a.at.side + b.at.side) <= SLACK, "sides differ");
        assert!(mathf::fabs(a.at.up - b.at.up) <= SLACK);
        assert!(mathf::fabs(a.at.forward - b.at.forward) <= SLACK);
    }
}

#[test]
fn an_outward_splay_is_signed_by_its_side() {
    // A roll swings a hanging limb toward the figure's left, so outward is a
    // different sign on each side and a shared limit would let one arm bend
    // into the ribs.
    let rig = built();
    let mut posture = Posture::rest(&rig);
    let outward = 2.0;
    assert!(posture
        .set(
            Bone::Shoulder(Side::Left).joint(),
            Rotation::new(0.0, 0.0, outward)
        )
        .is_ok());
    assert_eq!(
        posture.set(
            Bone::Shoulder(Side::Right).joint(),
            Rotation::new(0.0, 0.0, outward)
        ),
        Err(FigureError::RotationOutsideLimit),
        "the right arm's outward splay is the other sign"
    );
    assert!(posture
        .set(
            Bone::Shoulder(Side::Right).joint(),
            Rotation::new(0.0, 0.0, -outward)
        )
        .is_ok());
}

#[test]
fn a_hinge_joint_only_folds_one_way() {
    // The check that catches the elbow bending backwards, and the knee
    // bending forwards.
    let rig = built();
    let mut posture = Posture::rest(&rig);
    for side in Side::BOTH {
        assert!(
            posture
                .set(Bone::Elbow(side).joint(), Rotation::new(-1.5, 0.0, 0.0))
                .is_ok(),
            "an elbow folds the hand forward"
        );
        assert_eq!(
            posture.set(Bone::Elbow(side).joint(), Rotation::new(0.5, 0.0, 0.0)),
            Err(FigureError::RotationOutsideLimit),
            "an elbow must not open past straight"
        );
        assert!(
            posture
                .set(Bone::Knee(side).joint(), Rotation::new(1.5, 0.0, 0.0))
                .is_ok(),
            "a knee folds the shank backward"
        );
        assert_eq!(
            posture.set(Bone::Knee(side).joint(), Rotation::new(-0.5, 0.0, 0.0)),
            Err(FigureError::RotationOutsideLimit),
            "a knee must not fold forward"
        );
    }
}

#[test]
fn every_socket_the_plan_names_is_offered() {
    // Gear is parts on sockets, so a rig that offered fewer would silently
    // make a slot unequippable.
    let rig = built();
    for socket in Socket::ALL {
        assert!(rig.mount(socket).is_some(), "{socket:?} is not mounted");
    }
}

#[test]
fn the_hand_sockets_are_on_the_hands() {
    let rig = built();
    let main = rig.mount(Socket::MainHand).expect("mounted");
    let off = rig.mount(Socket::OffHand).expect("mounted");
    assert_eq!(main.joint, Bone::Wrist(Side::Right).joint());
    assert_eq!(off.joint, Bone::Wrist(Side::Left).joint());
}

#[test]
fn the_figure_places_at_every_heading() {
    // Sixteen headings from one rig, which is the artwork the body frame
    // saves: none of them is degenerate and none needs its own parts.
    let rig = built();
    let posture = Posture::rest(&rig);
    let mut out = Placement::new();
    for step in 0..16u32 {
        let facing = Facing(u16::try_from(step * 4096).expect("inside a turn"));
        posture
            .place(&stance(facing, 0.5, (40.0, 60.0)), &[], &mut out)
            .expect("places");
        assert_eq!(out.len(), rig.parts().len());
        for strip in out.strips() {
            for (x, y) in strip.near.iter().chain(strip.far) {
                // Saturation is how a surface placed off the canvas stays
                // off it, so nothing may ever reach the clamp.
                assert!(*x > i32::MIN && *x < i32::MAX);
                assert!(*y > i32::MIN && *y < i32::MAX);
            }
        }
    }
}

/// Every surface is drawn in a role, and a role is drawn in the colour the
/// record chose for it — the rig's tints are the identity's.
#[test]
fn a_figure_is_drawn_in_the_colours_its_record_chose() {
    for figure in &reference::FIGURES {
        let identity = figure.identity().expect("a real record");
        let rig = rig(&identity).expect("builds");
        assert_eq!(rig.tints(), identity.tints());
        let draws = |tint: Tint| rig.parts().iter().any(|part| part.tint() == tint);
        for always in [
            Tint::Skin,
            Tint::Eyes,
            Tint::Accent,
            Tint::Trousers,
            Tint::Leather,
        ] {
            assert!(draws(always), "{} draws nothing in {always:?}", figure.name);
        }
        let spec = identity.spec();
        assert_eq!(
            draws(Tint::Hair),
            spec.features.hair.is_some(),
            "{} draws hair it does not have, or has hair it does not draw",
            figure.name
        );
        assert_eq!(
            draws(Tint::Markings),
            !spec.species.markings().is_empty(),
            "a species' markings are drawn exactly when it has them ({})",
            figure.name
        );
    }
}

/// A palette edit is a re-tint, not a re-rig: swapping the colours moves no
/// point of any surface.
#[test]
fn retinting_a_figure_changes_its_colours_and_nothing_else() {
    let mut rig = built();
    let facing = Facing(0x2A00);
    let mut before = Placement::new();
    Posture::rest(&rig)
        .place(&stance(facing, 0.75, (30.0, 70.0)), &[], &mut before)
        .expect("places");
    rig.retint(Tints::new(
        [tairix_raster::Color::rgb(0x11, 0x22, 0x33); Tint::COUNT],
    ));
    let mut after = Placement::new();
    Posture::rest(&rig)
        .place(&stance(facing, 0.75, (30.0, 70.0)), &[], &mut after)
        .expect("places");
    let (one, other): (Vec<_>, Vec<_>) = (before.strips().collect(), after.strips().collect());
    assert_eq!(one.len(), other.len());
    for (was, now) in one.iter().zip(&other) {
        assert_eq!(
            (was.surface, was.near, was.far),
            (now.surface, now.near, now.far)
        );
    }
    assert!(one
        .iter()
        .zip(&other)
        .any(|(was, now)| was.color != now.color));
}

/// Every species at every build corner, with every form it may take, builds
/// — so no record the decoder admits is one the builder refuses — and never
/// needs more surfaces than a rig holds; the richest needs exactly that.
#[test]
fn every_admissible_figure_builds_within_the_part_bound() {
    let mut richest = 0;
    for species in Species::ALL {
        for (features, markings) in admissible(species) {
            for setting in [Setting::LOW, Setting::HIGH] {
                let spec = Spec {
                    species,
                    build: Build {
                        height: setting,
                        girth: setting,
                        taper: setting,
                        limbs: setting,
                        head: setting,
                    },
                    features,
                    palette: Palette {
                        markings,
                        eyes: species.eyes()[0],
                        ..Palette::default()
                    },
                };
                let identity = Identity::new(spec).expect("an admissible figure is a record");
                let rig = rig(&identity).expect("an admissible figure builds");
                assert!(rig.parts().len() >= BODY_PARTS);
                richest = richest.max(rig.parts().len());
            }
        }
    }
    assert_eq!(richest, MOST_PARTS, "the part bound is the richest figure");
}

/// Every figure a record describes reaches between [`LEAST_REACH`] and
/// [`MOST_REACH`], at any build corner with any feature it may carry, at
/// either end of its hair's volume — and the extremes are each within a unit
/// of their bound, so the shared frame spends none of its square on a figure
/// nobody can make and the readability floor is taken at a figure somebody
/// can.
#[test]
fn every_figure_reaches_between_the_least_and_the_most_reach() {
    let mut furthest: f64 = 0.0;
    let mut nearest = f64::MAX;
    for species in Species::ALL {
        for (features, markings) in admissible(species) {
            let volumes: &[Setting] = if features.hair.is_some() {
                &[Setting::LOW, Setting::HIGH]
            } else {
                &[Setting::LOW]
            };
            for volume in volumes {
                for mask in 0u8..32 {
                    let end = |bit: u8| {
                        if mask & (1 << bit) == 0 {
                            Setting::LOW
                        } else {
                            Setting::HIGH
                        }
                    };
                    let spec = Spec {
                        species,
                        build: Build {
                            height: end(0),
                            girth: end(1),
                            taper: end(2),
                            limbs: end(3),
                            head: end(4),
                        },
                        features: Features {
                            volume: *volume,
                            ..features
                        },
                        palette: Palette {
                            markings,
                            eyes: species.eyes()[0],
                            ..Palette::default()
                        },
                    };
                    let identity = Identity::new(spec).expect("an admissible figure");
                    let reach = rig(&identity).expect("it builds").reach();
                    assert!(reach <= MOST_REACH, "{spec:?} reaches {reach}");
                    assert!(reach >= LEAST_REACH, "{spec:?} reaches only {reach}");
                    furthest = mathf::fmax(furthest, reach);
                    nearest = mathf::fmin(nearest, reach);
                }
            }
        }
    }
    assert!(
        furthest > MOST_REACH - 1.0,
        "the furthest reach is {furthest}"
    );
    assert!(
        nearest < LEAST_REACH + 1.0,
        "the nearest reach is {nearest}"
    );
}

/// Every feature combination `species` admits, bald figures with no volume.
fn admissible(species: Species) -> Vec<(Features, u8)> {
    use crate::identity::{EyeShape, FaceShape, HairStyle};
    let mut out = Vec::new();
    for face in FaceShape::ALL {
        for eyes in EyeShape::ALL {
            for ears in species.ears() {
                for horns in species.horns().admitted() {
                    for tail in species.tails().admitted() {
                        for hair in HairStyle::ALL
                            .iter()
                            .map(|style| Some(*style))
                            .chain([None])
                        {
                            out.push((
                                Features {
                                    face: *face,
                                    eyes: *eyes,
                                    ears: *ears,
                                    horns,
                                    tail,
                                    hair,
                                    volume: Setting::LOW,
                                },
                                0,
                            ));
                        }
                    }
                }
            }
        }
    }
    out
}

/// Every figure has a tail's root, so every species shares one skeleton,
/// but only a tailed figure hangs anything on it.
#[test]
fn every_figure_has_a_tail_root_and_only_a_tailed_one_uses_it() {
    for figure in &reference::FIGURES {
        let identity = figure.identity().expect("a real record");
        let rig = rig(&identity).expect("builds");
        assert_eq!(rig.joints().len(), JOINT_COUNT);
        assert_eq!(
            rig.joints()[Bone::Tail.index()].parent,
            Some(Bone::Pelvis.joint())
        );
        let tailed = rig
            .parts()
            .iter()
            .any(|part| part.joint() == Bone::Tail.joint());
        assert_eq!(
            tailed,
            identity.spec().features.tail.is_some(),
            "{}",
            figure.name
        );
    }
}

/// A part's whole template is the record's to choose, never to write: the
/// skull a face is drawn as is one of the first-party skulls, found by name.
#[test]
fn a_skull_is_always_a_first_party_template() {
    use crate::identity::FaceShape;
    for face in FaceShape::ALL {
        let mut spec = reference::spec(Species::Human);
        spec.features.face = *face;
        let identity = Identity::new(spec).expect("real");
        let rig = rig(&identity).expect("builds");
        let skulls = rig
            .parts()
            .iter()
            .filter(|part: &&Part| part.rings() == feature::skull_for(*face))
            .count();
        assert_eq!(skulls, 1, "{face:?} is drawn as its own skull, once");
    }
}

/// The humanoid's cap of hair and the skull under it share one sort point
/// through their different stretches, so however the head nods or tilts
/// and whichever way the figure faces, the cap is painted over the skull.
#[test]
fn a_cap_of_hair_is_painted_over_its_skull_at_every_heading_and_nod() {
    use crate::pose::{Param, Pose};
    let identity = reference::identity(Species::Elf).expect("a haired figure");
    let rig = rig(&identity).expect("it builds");
    let rigging = super::rigging(&rig).expect("it binds");
    let skull = feature::skull_for(identity.spec().features.face);
    let skull_at = rig
        .parts()
        .iter()
        .position(|part| part.rings() == skull)
        .and_then(|at| u16::try_from(at).ok())
        .expect("the skull is there");
    // The cap is the hair authored after the skull; a mass down the back is
    // authored before it.
    let cap_at = rig
        .parts()
        .iter()
        .enumerate()
        .skip(usize::from(skull_at) + 1)
        .find(|(_, part)| part.tint() == Tint::Hair)
        .and_then(|(at, _)| u16::try_from(at).ok())
        .expect("a cap over the crown");
    let mut out = Placement::new();
    for step in 0..16u32 {
        let facing = Facing(u16::try_from(step * 4096).expect("inside a turn"));
        for (nod, tilt) in [
            (-1.0, 0.0),
            (-0.5, 0.5),
            (0.0, 0.0),
            (0.5, -0.5),
            (1.0, 1.0),
        ] {
            let pose = Pose::REST
                .with(Param::HeadNod, nod)
                .and_then(|pose| pose.with(Param::HeadTilt, tilt))
                .expect("in range");
            rigging
                .posture(&pose)
                .expect("posturable")
                .place(&stance(facing, 1.0, (0.0, 0.0)), &[], &mut out)
                .expect("places");
            let painted: Vec<u16> = out.strips().map(|strip| strip.surface).collect();
            let first = |surface: u16| painted.iter().position(|at| *at == surface);
            assert!(
                first(cap_at) > first(skull_at),
                "the cap went under the skull facing {facing:?} at nod {nod}, tilt {tilt}"
            );
        }
    }
}
