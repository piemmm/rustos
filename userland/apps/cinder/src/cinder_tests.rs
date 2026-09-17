//! Body tests: the skeleton's shape, the pose parameters, and the sort that
//! turns the creature round.

use super::{parts, Eyes, Pose, PART_COUNT, STANDING_HEIGHT};
use crate::project::Ground;
use crate::shape::{Placed, Shape};
use tairix_util::mathf;

fn standing() -> Pose {
    Pose {
        at: Ground::new(200.0, 200.0),
        ..Pose::default()
    }
}

/// The ink nose: the only ink mass that is wider than it is tall, so it is
/// findable without the skeleton exposing its indices. The pupils are ink too
/// but are tall, and the mouth's points are round.
fn nose(list: &[Placed]) -> Option<usize> {
    list.iter().position(|part| {
        part.color == super::palette::INK
            && matches!(part.shape, Shape::Mass { rx, ry, .. } if rx > ry)
    })
}

/// A part's half-height, whatever shape it is drawn as.
fn half_height(part: &Placed) -> f64 {
    match part.shape {
        Shape::Fur { radius } => radius,
        Shape::Mass { ry, .. } | Shape::Drape { ry, .. } => ry,
        Shape::Limb { length, foot, .. } => length + foot,
        Shape::Ear { height, .. } => height,
    }
}

#[test]
fn every_skeleton_part_is_placed() {
    assert_eq!(parts(&standing()).len(), PART_COUNT);
}

#[test]
fn a_lid_flattens_the_eyes_rather_than_removing_them() {
    let open = parts(&standing());
    let shut = parts(&Pose {
        eyes: Eyes::Shut,
        ..standing()
    });
    assert_eq!(
        open.len(),
        shut.len(),
        "a shut eye is a closed lid, never a hole left in the mask"
    );
    let tallest = |list: &[Placed], colour| {
        list.iter()
            .filter(|part| part.color == colour)
            .map(half_height)
            .fold(0.0_f64, f64::max)
    };
    let amber = super::palette::EYE_AMBER;
    assert!(
        tallest(&shut, amber) < tallest(&open, amber) * 0.2,
        "a shut lid leaves a sliver of iris, not a full eye"
    );
}

#[test]
fn a_lid_narrows_an_eye_without_narrowing_it_sideways() {
    // A lid comes *down*: an eye that shrank in both directions would read as
    // the creature's eyes getting smaller rather than closing.
    let widest = |eyes| {
        parts(&Pose { eyes, ..standing() })
            .into_iter()
            .filter(|part| part.color == super::palette::EYE_AMBER)
            .map(|part| match part.shape {
                Shape::Mass { rx, .. } => rx,
                _ => 0.0,
            })
            .fold(0.0_f64, f64::max)
    };
    assert!((widest(Eyes::Open) - widest(Eyes::Half)).abs() < f64::EPSILON);
    assert!((widest(Eyes::Open) - widest(Eyes::Shut)).abs() < f64::EPSILON);
}

#[test]
fn the_paint_order_is_a_function_of_the_pose_alone() {
    // Two frames of the same pose must paint identically, or the coat flickers.
    let order = |pose: &Pose| {
        parts(pose)
            .into_iter()
            .map(|part| part.seed)
            .collect::<alloc::vec::Vec<_>>()
    };
    let pose = Pose {
        heading: 0.9,
        ..standing()
    };
    assert_eq!(order(&pose), order(&pose));
}

#[test]
fn parts_at_the_same_depth_paint_in_the_order_they_were_authored() {
    // The cape's flap and its piping sit at the same depth whenever he faces
    // the camera — the commonest heading there is — so only the tie-break
    // keeps the piping behind the flap it edges. A tie broken by a scrambled
    // seed would put the trim in front some of the time.
    let list = parts(&Pose {
        heading: -core::f64::consts::FRAC_PI_2,
        ..standing()
    });
    let flap: alloc::vec::Vec<(usize, tairix_raster::Color)> = list
        .iter()
        .enumerate()
        .filter(|(_, part)| matches!(part.shape, Shape::Mass { square, .. } if square > 0.7))
        .map(|(index, part)| (index, part.color))
        .collect();
    assert_eq!(flap.len(), 2, "the flap and its piping");
    assert_eq!(
        flap[0].1,
        super::palette::FUR_MID,
        "the piping is authored first and must paint first"
    );
    assert_eq!(flap[1].1, super::palette::SLATE_DEEP);
}

#[test]
fn the_face_leads_facing_the_camera_and_trails_facing_away() {
    let facing = parts(&Pose {
        heading: -core::f64::consts::FRAC_PI_2,
        ..standing()
    });
    let away = parts(&Pose {
        heading: core::f64::consts::FRAC_PI_2,
        ..standing()
    });
    let (Some(front), Some(back)) = (nose(&facing), nose(&away)) else {
        unreachable!("the nose is in the skeleton at both headings");
    };
    assert!(
        front > back,
        "facing the camera the nose paints late (in front); facing away it paints early (behind)"
    );
}

#[test]
fn a_part_keeps_its_ripple_between_frames() {
    // The same part must carry the same seed every frame, or the coat boils.
    let first = parts(&standing());
    let second = parts(&Pose {
        gait_phase: 1.2,
        ..standing()
    });
    let mut first_seeds: alloc::vec::Vec<u16> = first.iter().map(|part| part.seed).collect();
    let mut second_seeds: alloc::vec::Vec<u16> = second.iter().map(|part| part.seed).collect();
    first_seeds.sort_unstable();
    second_seeds.sort_unstable();
    assert_eq!(first_seeds, second_seeds);
}

#[test]
fn the_creature_stands_about_as_tall_as_it_says_it_does() {
    let list = parts(&standing());
    let top = list
        .iter()
        .map(|part| part.y - half_height(part))
        .fold(f64::INFINITY, f64::min);
    let height = standing().at.y - top;
    assert!(
        mathf::fabs(height - STANDING_HEIGHT) < STANDING_HEIGHT * 0.25,
        "the authored height and the drawn height must stay in step (drawn {height})"
    );
}

#[test]
fn a_crouch_lowers_the_body_and_a_lift_raises_it() {
    let base = parts(&standing());
    let crouched = parts(&Pose {
        crouch: 1.0,
        ..standing()
    });
    let lifted = parts(&Pose {
        lift: 20.0,
        ..standing()
    });
    let highest = |list: &[Placed]| list.iter().map(|part| part.y).fold(f64::INFINITY, f64::min);
    assert!(highest(&crouched) > highest(&base), "a crouch flattens");
    assert!(highest(&lifted) < highest(&base), "a jump rises");
}

#[test]
fn turning_the_head_moves_the_muzzle_without_moving_the_feet() {
    let square = standing();
    let turned = Pose {
        head_yaw: 0.6,
        ..square
    };
    assert_eq!(square.at, turned.at);
    let at = |pose: &Pose| {
        let list = parts(pose);
        nose(&list).map(|index| (list[index].x, list[index].y))
    };
    assert_ne!(at(&square), at(&turned), "the head must actually turn");
}

#[test]
fn the_eyes_read_against_the_cream_mask() {
    // The sheet's face works because the amber sits inside a lash line on a
    // cream mask. Without the line the eye is amber-on-cream and disappears at
    // a companion's size, which is what the first attempt did.
    let list = parts(&standing());
    let pick = |colour| {
        list.iter()
            .find(|part| part.color == colour)
            .map(half_height)
    };
    let (Some(ring), Some(iris)) = (
        pick(super::palette::EYE_RING),
        pick(super::palette::EYE_AMBER),
    ) else {
        unreachable!("each eye is ringed and has an iris");
    };
    assert!(
        ring > iris,
        "the lash line must show around the iris, not behind it"
    );
}

#[test]
fn the_eyes_are_taller_than_they_are_wide() {
    // The sheet's eyes are the character: tall rounded almonds, not discs.
    for part in parts(&standing()) {
        if part.color != super::palette::EYE_AMBER {
            continue;
        }
        let Shape::Mass { rx, ry, .. } = part.shape else {
            unreachable!("an iris is a rounded mass");
        };
        assert!(ry > rx, "an iris {ry} tall must be wider than {rx} across");
    }
}

#[test]
fn the_face_carries_a_smile_that_follows_the_mood() {
    let level = parts(&Pose {
        smile: 0.0,
        ..standing()
    });
    let grinning = parts(&Pose {
        smile: 1.0,
        ..standing()
    });
    assert_eq!(
        level.len(),
        grinning.len(),
        "a smile moves the mouth, not the face"
    );

    // The corners lift; the centre of the mouth does not move nearly as much,
    // which is what makes it a curve rather than a mouth sliding upwards.
    let lowest = |list: &[Placed]| {
        list.iter()
            .filter(|part| {
                part.color == super::palette::INK
                    && matches!(part.shape, Shape::Mass { rx, .. } if rx < 1.5)
            })
            .map(|part| part.y)
            .fold(f64::NEG_INFINITY, f64::max)
    };
    assert!(
        lowest(&grinning) < lowest(&level),
        "a grin must raise the mouth's corners"
    );
}

#[test]
fn the_tail_is_banded_rather_than_one_colour() {
    // The ringed tail is the other thing the eye picks out of the sheet, and
    // it is fur rather than an outline, because a ring with a crisp edge reads
    // as a bangle.
    let list = parts(&standing());
    let banded = |colour| {
        list.iter()
            .filter(|part| part.color == colour && matches!(part.shape, Shape::Fur { .. }))
            .count()
    };
    let bright = banded(super::palette::FUR_BRIGHT);
    let deep = banded(super::palette::FUR_DEEP);
    assert!(
        bright >= 3 && deep >= 3,
        "the tail alternates light and dark rings ({bright} light, {deep} dark)"
    );
}

#[test]
fn his_body_is_rust_and_only_the_garment_is_dark() {
    // The correction the sheet actually states: the dark mass on screen is a
    // *cloak*, not his body. A trunk painted in the cloth's tones would be a
    // black cat in a black coat.
    let cloth = [
        super::palette::SLATE,
        super::palette::SLATE_SHADE,
        super::palette::SLATE_DEEP,
    ];
    let coat = [
        super::palette::FUR_BRIGHT,
        super::palette::FUR_MID,
        super::palette::FUR_DEEP,
    ];
    let list = parts(&standing());
    let in_cloth = list
        .iter()
        .filter(|part| cloth.contains(&part.color))
        .count();
    let in_coat = list
        .iter()
        .filter(|part| coat.contains(&part.color))
        .count();
    assert!(
        in_cloth > 0,
        "he wears a cowl and a cape; they have to be drawn"
    );
    assert!(
        in_coat > in_cloth,
        "his coat ({in_coat} parts) must outweigh his costume ({in_cloth} parts)"
    );
}

#[test]
fn the_garment_is_cloth_rather_than_a_card_taped_on() {
    // A garment drawn as a flat quadrilateral is what made the first attempt
    // read as a black slab. Every panel of it hangs in folds.
    let list = parts(&standing());
    let drapes = list
        .iter()
        .filter(|part| matches!(part.shape, Shape::Drape { .. }))
        .count();
    assert!(drapes >= 3, "the cape and the cowl are draped panels");
}

#[test]
fn each_limb_grows_out_of_a_joint_that_belongs_to_the_body() {
    // The defect this guards: limbs that float. A shank's outline starts at
    // its joint, and the trunk carries a mass at that very point, so a swing
    // can never open a gap at the shoulder.
    let list = parts(&standing());
    let shanks: alloc::vec::Vec<&Placed> = list
        .iter()
        .filter(|part| matches!(part.shape, Shape::Limb { .. }))
        .collect();
    assert_eq!(shanks.len(), 4, "four legs");
    for shank in shanks {
        let covered = list.iter().any(|part| {
            !matches!(part.shape, Shape::Limb { .. })
                && mathf::hypot(part.x - shank.x, part.y - shank.y) < half_height(part)
        });
        assert!(
            covered,
            "the joint at ({}, {}) must sit inside a body part",
            shank.x, shank.y
        );
    }
}

#[test]
fn a_walking_limb_pivots_in_its_joint_instead_of_sliding() {
    // A limb that only translated read as a peg sliding under the body. Its
    // joint must hold still through the stride while its outline turns.
    let mid_stride = Pose {
        gait_phase: core::f64::consts::FRAC_PI_2,
        ..standing()
    };
    let shanks = |pose: &Pose| {
        parts(pose)
            .into_iter()
            .filter(|part| matches!(part.shape, Shape::Limb { .. }))
            .map(|part| (part.x, part.y, part.turn))
            .collect::<alloc::vec::Vec<_>>()
    };
    let square = shanks(&standing());
    let stepping = shanks(&mid_stride);
    assert_eq!(square.len(), stepping.len());
    assert!(
        stepping.iter().any(|&(_, _, turn)| turn.abs() > 0.05),
        "a stride must actually turn a limb"
    );
}

#[test]
fn a_limb_stops_turning_on_screen_when_he_walks_towards_the_camera() {
    // Facing the camera a leg swings in *depth*, so its billboard must not
    // rotate: the projection decides that, not a branch on which way he faces.
    let towards = Pose {
        heading: -core::f64::consts::FRAC_PI_2,
        gait_phase: core::f64::consts::FRAC_PI_2,
        ..standing()
    };
    for part in parts(&towards) {
        if matches!(part.shape, Shape::Limb { .. }) {
            assert!(
                part.turn.abs() < 1.0e-9,
                "a limb swinging in depth must not turn on screen"
            );
        }
    }
}

#[test]
fn the_ears_lean_apart_rather_than_both_the_same_way() {
    let list = parts(&standing());
    let leans: alloc::vec::Vec<f64> = list
        .iter()
        .filter_map(|part| match part.shape {
            Shape::Ear { lean, .. } => Some(lean),
            _ => None,
        })
        .collect();
    assert!(!leans.is_empty(), "he has ears");
    assert!(
        leans.iter().any(|&lean| lean > 0.0) && leans.iter().any(|&lean| lean < 0.0),
        "a pair leaning the same way reads as a hat, not as ears"
    );
}

#[test]
fn the_body_fits_inside_the_surface_the_companion_asks_for() {
    // The bound the desktop enforces is on the surface; this is the body being
    // held to it, derived from the skeleton rather than asserted about it.
    let side = f64::from(crate::layout::COMPANION_SIDE);
    assert!(
        super::reach() * 2.0 < side,
        "the creature reaches {} and the surface is {side} across",
        super::reach()
    );
}
