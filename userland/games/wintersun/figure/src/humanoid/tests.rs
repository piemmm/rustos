//! The humanoid's own consistency: its proportions, its hierarchy, and the
//! rule that a joint bearing a limb carries its mass.

use tairix_raster::shape::Outline;
use tairix_util::mathf;
use tairix_wintersun_net::value::Facing;

use super::{palette, rig, Bone, JOINT_COUNT, PART_COUNT, STANDING_HEIGHT};
use crate::error::FigureError;
use crate::frame::{Rotation, FORESHORTEN};
use crate::rig::{Placement, Posture, Rig};
use crate::socket::{Side, Socket};

const EAST: Facing = Facing(0);
const SOUTH: Facing = Facing(0x4000);

const SLACK: f64 = 1e-9;

fn built() -> Rig {
    rig().expect("the shipped humanoid must be consistent")
}

#[test]
fn the_shipped_rig_assembles() {
    // Every check `Rig::new` makes, applied to the rig the game draws — so
    // editing this table into something inconsistent fails here.
    let rig = built();
    assert_eq!(rig.joints().len(), JOINT_COUNT);
    assert_eq!(rig.parts().len(), PART_COUNT);
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
                .filter(|part| part.joint == bearing.joint())
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
        .place(SOUTH, 1.0, (0.0, 0.0), &[], &mut out)
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
            .filter(|(_, part)| part.joint == bearing.joint())
            .map(|(index, _)| u16::try_from(index).expect("inside the bound"));
        let cap = seeds.next().expect("the cap");
        let limb = seeds.next().expect("the limb");
        let cap = out.parts().find(|p| p.seed == cap).expect("placed");
        let limb = out.parts().find(|p| p.seed == limb).expect("placed");
        assert!(
            mathf::fabs(cap.x - limb.x) <= SLACK && mathf::fabs(cap.y - limb.y) <= SLACK,
            "{bearing:?} parted its mass from its limb"
        );
    }
}

#[test]
fn the_figure_stands_its_stated_height() {
    // `STANDING_HEIGHT` is what a caller's scale is computed against, so it
    // has to be the height the rig actually draws rather than a label.
    // Measured in the figure's own frame, and off the traced outlines. Not
    // off a shape's reach, which is a radial bound and over-states a tall
    // shape's height by its own width; and not off the placed rows, whose
    // spread includes the depth between the two feet.
    let rig = built();
    let mut above = [0.0_f64; JOINT_COUNT];
    for (index, joint) in rig.joints().iter().enumerate() {
        // At rest every frame is square, so a joint's height is the sum of
        // the offsets up its parent chain — and a parent always precedes it.
        let carried = joint.parent.map_or(0.0, |parent| above[parent.index()]);
        above[index] = carried + joint.at.up;
    }

    let mut outline = Outline::new();
    let mut crown = 0.0;
    let mut sole = 0.0;
    for part in rig.parts() {
        part.shape.trace(&mut outline);
        assert!(!outline.is_empty(), "the humanoid draws no untraced shape");
        let base = above[part.joint.index()] + part.at.up;
        for &(_, up) in &outline {
            crown = mathf::fmax(crown, base + up);
            sole = mathf::fmin(sole, base + up);
        }
    }

    let height = crown - sole;
    assert!(
        mathf::fabs(height - STANDING_HEIGHT) < STANDING_HEIGHT * 0.01,
        "the rig stands {height} against a stated {STANDING_HEIGHT}"
    );
    assert!(
        mathf::fabs(sole) < STANDING_HEIGHT * 0.01,
        "the feet must meet the ground, not sit {sole} from it"
    );
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
        .place(EAST, 1.0, (0.0, 0.0), &[], &mut out)
        .expect("places");

    let row_of = |bone: Bone| {
        let seed = rig
            .parts()
            .iter()
            .position(|part| part.joint == bone.joint())
            .and_then(|index| u16::try_from(index).ok())
            .expect("the ankle carries a part");
        out.parts()
            .find(|part| part.seed == seed)
            .expect("placed")
            .y
    };

    let far = row_of(Bone::Ankle(Side::Left));
    let near = row_of(Bone::Ankle(Side::Right));
    assert!(far < near, "the far foot must draw higher up the screen");
    let apart = near - far;
    let hips_apart = 18.0;
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
            .place(facing, 0.5, (40.0, 60.0), &[], &mut out)
            .expect("places");
        assert_eq!(out.len(), PART_COUNT);
        for part in out.parts() {
            assert!(part.x.is_finite() && part.y.is_finite() && part.turn.is_finite());
        }
    }
}

#[test]
fn the_palette_is_distinct() {
    // A re-tint that collapsed two tones would flatten the figure without
    // failing anything else.
    let tones = [
        palette::SKIN_LIT,
        palette::SKIN_MID,
        palette::SKIN_SHADE,
        palette::CLOTH_LIT,
        palette::CLOTH_SHADE,
        palette::LEATHER,
    ];
    for (index, tone) in tones.iter().enumerate() {
        for other in &tones[index + 1..] {
            assert_ne!(tone, other, "two tones are the same colour");
        }
    }
}
