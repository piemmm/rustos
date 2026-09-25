//! What the reference grid is, and the property that makes it one grid.

use tairix_util::mathf;

use alloc::vec;
use alloc::vec::Vec;

use tairix_raster::surface::SUBPIXEL;
use tairix_wintersun_net::value::Facing;

use super::{
    allowance, fit, framing, grid, side_at, spec, Cell, Figure, Reference, Sampling, BREATH,
    FACINGS, FIGURES, MARGIN, PHASES, SAMPLES, SIDES, STANDING,
};
use crate::breath::Breath;
use crate::frame::project;
use crate::humanoid;
use crate::identity::{
    EarForm, EyeShape, FaceShape, Features, HairStyle, HornForm, Identity, Setting, TailForm,
};
use crate::motion::{self, Kind};
use crate::rig::Frames;
use crate::rig::Placement;
use crate::species::Species;
use crate::testing::corners;

const SCALE: f64 = 1.25;
const AT: (f64, f64) = (37.5, 92.25);

/// Every figure of the grid, the generated ones included.
fn whole() -> Vec<Figure<'static>> {
    grid()
        .map(|figure| figure.expect("every sample draws a record"))
        .collect()
}

/// A figure's cells walk its motions, the phases and the headings exactly
/// once each: every motion for a species' reference, the walk alone for a
/// least or a most.
#[test]
fn every_figure_walks_its_own_cells_once_each() {
    for figure in &whole() {
        let kinds = figure.kinds();
        match figure.sampling {
            Sampling::Every => assert_eq!(kinds, &Kind::ALL[..]),
            Sampling::Walk => assert_eq!(kinds, &[Kind::Walk]),
        }
        let mut seen = vec![false; Kind::ALL.len() * PHASES * FACINGS.len()];
        let mut count = 0;
        for cell in figure.cells() {
            assert!(kinds.contains(&cell.kind));
            let slot = cell.kind.index() * PHASES * FACINGS.len()
                + cell.step * FACINGS.len()
                + FACINGS
                    .iter()
                    .position(|f| *f == cell.facing)
                    .expect("a listed heading");
            assert!(!seen[slot], "{} repeats a cell", figure.name);
            seen[slot] = true;
            count += 1;
        }
        assert_eq!(count, kinds.len() * PHASES * FACINGS.len());
    }
}

/// Every figure the grid draws is a record a client could have sent: the
/// grid is never exercising a figure the decoder would refuse.
#[test]
fn every_figure_of_the_grid_is_a_real_record() {
    for figure in &whole() {
        let identity = figure.identity().expect("a grid figure is a real record");
        assert_eq!(
            Identity::decode(&identity.encode()),
            Ok(identity),
            "{} does not survive its own record",
            figure.name
        );
    }
}

/// The five references come first, one per species in species order, and
/// every name is its own — a ledger row and a sheet are found by it.
#[test]
fn the_references_lead_one_per_species_and_names_are_unique() {
    for (species, figure) in Species::ALL.iter().zip(&FIGURES) {
        assert_eq!(figure.spec.species, *species);
        assert_eq!(figure.sampling, Sampling::Every);
        assert_eq!(figure.name, species.name());
        assert_eq!(figure.spec, spec(*species));
    }
    for (index, figure) in FIGURES.iter().enumerate() {
        assert!(
            FIGURES[index + 1..]
                .iter()
                .all(|other| other.name != figure.name),
            "{} is named twice",
            figure.name
        );
    }
}

/// Between them the grid's figures wear every form there is — every ear,
/// horn, tail, face, eye shape and hair style, and the want of each
/// optional one — so a bound is held on every template rather than on the
/// few somebody happened to pick.
#[test]
fn the_grid_wears_every_form() {
    let worn = |wears: &dyn Fn(&Features) -> bool| {
        FIGURES.iter().any(|figure| wears(&figure.spec.features))
    };
    for ears in EarForm::ALL {
        assert!(worn(&|f| f.ears == *ears), "{ears:?}");
    }
    for horns in HornForm::ALL.iter().map(|form| Some(*form)).chain([None]) {
        assert!(worn(&|f| f.horns == horns), "{horns:?}");
    }
    for tail in TailForm::ALL.iter().map(|form| Some(*form)).chain([None]) {
        assert!(worn(&|f| f.tail == tail), "{tail:?}");
    }
    for hair in HairStyle::ALL
        .iter()
        .map(|style| Some(*style))
        .chain([None])
    {
        assert!(worn(&|f| f.hair == hair), "{hair:?}");
    }
    for face in FaceShape::ALL {
        assert!(worn(&|f| f.face == *face), "{face:?}");
    }
    for eyes in EyeShape::ALL {
        assert!(worn(&|f| f.eyes == *eyes), "{eyes:?}");
    }
}

/// Each species is drawn at both ends of its build as well as its middle,
/// and in the palest and darkest covering it admits, so a bound holds across
/// the whole of what a record can ask for.
#[test]
fn every_species_is_drawn_at_both_ends_of_its_build_and_its_skin() {
    // Channel sum: the order a pale and a dark swatch are told apart by,
    // which is all this needs.
    let brightness = |colour: tairix_raster::Color| {
        u32::from(colour.r) + u32::from(colour.g) + u32::from(colour.b)
    };
    for species in Species::ALL {
        let of_species = || FIGURES.iter().filter(move |f| f.spec.species == species);
        let at = |setting: Setting| {
            of_species().any(|f| {
                let build = f.spec.build;
                [
                    build.height,
                    build.girth,
                    build.taper,
                    build.limbs,
                    build.head,
                ]
                .into_iter()
                .all(|held| held == setting)
            })
        };
        assert!(at(Setting::LOW), "{species:?} is not drawn at its least");
        assert!(at(Setting::HIGH), "{species:?} is not drawn at its most");

        let covering = species.covering();
        let lit = |index: &usize| brightness(covering[*index]);
        let palest = (0..covering.len()).max_by_key(lit).expect("a swatch");
        let darkest = (0..covering.len()).min_by_key(lit).expect("a swatch");
        let worn: Vec<usize> = of_species()
            .map(|f| usize::from(f.spec.palette.skin))
            .collect();
        assert!(
            worn.contains(&palest),
            "{species:?} is not drawn in its palest"
        );
        assert!(
            worn.contains(&darkest),
            "{species:?} is not drawn in its darkest"
        );
    }
}

/// The heading picks which way the figure is seen, never what it is doing —
/// otherwise a contact sheet's four columns would be four animations.
#[test]
fn the_pose_does_not_depend_on_the_heading() {
    for figure in &whole() {
        let reference =
            Reference::new(&figure.identity().expect("a real record")).expect("it builds");
        let mut placement = Placement::new();
        for kind in figure.kinds() {
            for step in 0..PHASES {
                let mut root = None;
                for facing in FACINGS {
                    let cell = Cell {
                        kind: *kind,
                        step,
                        facing,
                    };
                    let planted = reference
                        .place(cell, SCALE, AT, &mut placement)
                        .expect("it places");
                    match root {
                        None => root = Some(planted),
                        Some(first) => assert_eq!(
                            first.pose(),
                            planted.pose(),
                            "{} {} at {step} poses differently facing {facing:?}",
                            figure.name,
                            kind.name()
                        ),
                    }
                }
            }
        }
    }
}

/// A sheet is a drawing of the clip, so on the level ground the grid stands
/// on the planting solve must put the planted foot down and change nothing
/// else — for every figure, whatever its build.
#[test]
fn every_cell_stands_on_the_ground_it_was_given() {
    let mut placement = Placement::new();
    for figure in &whole() {
        let reference =
            Reference::new(&figure.identity().expect("a real record")).expect("it builds");
        for cell in figure.cells() {
            let planted = reference
                .place(cell, SCALE, AT, &mut placement)
                .expect("it places");
            assert!(
                planted.worst_miss() < 1e-6,
                "{} {cell:?} missed by {}",
                figure.name,
                planted.worst_miss()
            );
            let clip = reference.clip(cell.kind).expect("a shipped clip");
            let asked = clip.root_at(cell.phase()) * reference.legs().straight();
            assert!(
                mathf::fabs(planted.root().at.up - asked) <= 1e-9,
                "{} {cell:?} stands at {} where its clip asked for {asked}",
                figure.name,
                planted.root().at.up
            );
            assert!(!placement.is_empty());
        }
    }
}

/// The grid is the authored figures in their order and then the samples in
/// theirs, every name its own across the whole of it.
#[test]
fn the_grid_is_the_authored_figures_then_the_samples() {
    let whole = whole();
    assert_eq!(whole.len(), FIGURES.len() + SAMPLES.len());
    for (listed, authored) in whole.iter().zip(&FIGURES) {
        assert_eq!(listed.name, authored.name);
        assert_eq!(listed.spec, authored.spec);
    }
    for (listed, sample) in whole[FIGURES.len()..].iter().zip(&SAMPLES) {
        assert_eq!(listed.name, sample.name);
        assert_eq!(listed.spec.species, sample.species);
    }
    for (index, figure) in whole.iter().enumerate() {
        assert!(
            whole[index + 1..]
                .iter()
                .all(|other| other.name != figure.name),
            "{} is named twice",
            figure.name
        );
    }
}

/// A sample is exactly what the generator draws from its seed, walking, and
/// every species is sampled twice with seeds of its own.
#[test]
fn every_sample_is_the_generators_own_draw() {
    for sample in &SAMPLES {
        let drawn = crate::plausible::figure(
            sample.species,
            &mut tairix_rng::NonCryptoRng::seed_from_u64(sample.seed),
        )
        .expect("every draw is a record");
        let figure = sample.figure().expect("every draw is a record");
        assert_eq!(figure.spec, drawn.spec(), "{}", sample.name);
        assert_eq!(figure.sampling, Sampling::Walk);
    }
    for species in Species::ALL {
        assert_eq!(
            SAMPLES.iter().filter(|s| s.species == species).count(),
            2,
            "{species:?}"
        );
    }
    for (index, sample) in SAMPLES.iter().enumerate() {
        assert!(SAMPLES[index + 1..]
            .iter()
            .all(|other| other.seed != sample.seed));
    }
}

/// The regression the framing was missing: every cell of the grid, at every
/// side it is drawn at, lies inside its square. A near foot striding toward
/// the viewer is drawn below the ground point it stands on, and a frame that
/// left room only for the figure's height cut it off in over half the cells.
#[test]
fn every_cell_of_the_grid_lies_inside_its_square() {
    let mut placement = Placement::new();
    for figure in &whole() {
        let reference =
            Reference::new(&figure.identity().expect("a real record")).expect("it builds");
        for side in SIDES {
            let edge = i32::try_from(side).expect("a small side") * SUBPIXEL;
            for cell in figure.cells() {
                let (scale, at) =
                    fit(cell.kind, reference.rig().reach(), side).expect("a real cell");
                reference
                    .place(cell, scale, at, &mut placement)
                    .expect("it places");
                for strip in placement.strips() {
                    for (x, y) in strip.near.iter().chain(strip.far) {
                        assert!(
                            (0..=edge).contains(x) && (0..=edge).contains(y),
                            "{} {cell:?} at {side} draws ({x}, {y}) outside its square",
                            figure.name
                        );
                    }
                }
            }
        }
    }
}

/// Every build corner of every species, in every shipped motion at each of
/// the grid's phases, at either extreme of the breath and every sixteenth of
/// a turn, stays within [`ABOVE`] and [`BELOW`] of its ground point —
/// measured off the whole outline of every ring, of which a strip draws the
/// near half — and the furthest any reaches is within a hundredth of each,
/// so the frame spends none of its square on a figure nobody can make.
///
/// A pose does not depend on the heading, so each is carried once and its
/// rings projected at every heading.
#[test]
fn every_build_stays_within_the_allowances() {
    let motions = motion::Set::new().expect("the shipped set");
    let breaths = [0.25, 0.75].map(|share| {
        let mut breath = Breath::new(BREATH.0, BREATH.1).expect("the stage's breath");
        breath.advance(share * BREATH.0).expect("a real step");
        breath
    });
    let mut frames = Frames::new();
    let mut reached = [(0.0f64, 0.0f64); Kind::ALL.len()];
    for species in Species::ALL {
        for identity in corners(species) {
            let reference = Reference::new(&identity).expect("it builds");
            let rigging = humanoid::rigging(reference.rig()).expect("it binds");
            let reach = reference.rig().reach();
            for (kind, step, breath) in Kind::ALL.into_iter().flat_map(|kind| {
                (0..PHASES).flat_map(move |step| breaths.map(|breath| (kind, step, breath)))
            }) {
                let clip = motions.clip(kind).expect("a shipped clip");
                let phase = Cell {
                    kind,
                    step,
                    facing: FACINGS[0],
                }
                .phase();
                let planted = reference
                    .staged
                    .plant(
                        &clip.sample(phase).expect("in the clip"),
                        breath,
                        clip.root_at(phase),
                    )
                    .expect("it plants");
                rigging
                    .posture(&planted.pose())
                    .expect("in its limits")
                    .resolve(planted.root(), &mut frames);
                for part in reference.rig().parts() {
                    for hoop in reference.surfaces(part, &frames).expect("it carries") {
                        for turn in 0u16..16 {
                            let facing = Facing(turn * 0x1000);
                            let centre = project(facing, hoop.at).dy;
                            let spread = mathf::hypot(
                                project(facing, hoop.wide).dy,
                                project(facing, hoop.deep).dy,
                            );
                            let slot = &mut reached[kind.index()];
                            slot.0 = mathf::fmax(slot.0, (spread - centre) / reach);
                            slot.1 = mathf::fmax(slot.1, (centre + spread) / reach);
                        }
                    }
                }
            }
        }
    }
    // Locomotion shares one framing, so it is held tight to the furthest of
    // the three; any other motion is held tight wherever it needs more room
    // than locomotion, and framed as locomotion is wherever it does not.
    let standing =
        [Kind::Idle, Kind::Walk, Kind::Run]
            .into_iter()
            .fold((0.0f64, 0.0f64), |held, kind| {
                let (above, below) = reached[kind.index()];
                (mathf::fmax(held.0, above), mathf::fmax(held.1, below))
            });
    for kind in Kind::ALL {
        let (above, below) = reached[kind.index()];
        let (room_above, room_below) = allowance(kind);
        let name = kind.name();
        assert!(above <= room_above, "{name} drawn {above} above");
        assert!(below <= room_below, "{name} drawn {below} below");
        let (tight_above, tight_below) = if kind.layer() == crate::motion::Layer::Locomotion {
            standing
        } else {
            (above, below)
        };
        if room_above > STANDING.0 || kind.layer() == crate::motion::Layer::Locomotion {
            assert!(
                room_above - tight_above <= 0.01,
                "{name} has room above to spare"
            );
        }
        if room_below > STANDING.1 || kind.layer() == crate::motion::Layer::Locomotion {
            assert!(
                room_below - tight_below <= 0.01,
                "{name} has room below to spare"
            );
        }
        assert!(
            room_above >= STANDING.0 && room_below >= STANDING.1,
            "{name}"
        );
    }
}

/// No motion is framed tighter than locomotion, which is what makes the
/// inverse of the framing at locomotion's the conservative one.
#[test]
fn no_motion_is_framed_tighter_than_locomotion() {
    let (tightest, _) = framing(Kind::Idle);
    for kind in Kind::ALL {
        let (share, _) = framing(kind);
        assert!(share <= tightest, "{} is framed tighter", kind.name());
    }
    for side in SIDES {
        let (scale, _) = fit(Kind::Walk, 64.0, side).expect("a real cell");
        assert!(mathf::fabs(side_at(64.0, scale) - f64::from(side)) < 1e-9);
    }
}

/// A figure fills the same share of every cell side and stands at the same
/// place down it, the whole of what it may draw between the margins, and a
/// reach or a side no figure fits is refused.
#[test]
fn a_cell_frames_a_figure_by_its_own_reach() {
    for kind in Kind::ALL {
        let (above, below) = allowance(kind);
        for side in SIDES {
            let (scale, at) = fit(kind, 50.0, side).expect("a real cell");
            let extent = f64::from(side);
            let top = at.1 - above * 50.0 * scale;
            let bottom = at.1 + below * 50.0 * scale;
            assert!(mathf::fabs(top - MARGIN * extent) < 1e-9);
            assert!(mathf::fabs(bottom - (1.0 - MARGIN) * extent) < 1e-9);
            assert!(mathf::fabs(at.0 - extent * 0.5) < 1e-12);
        }
    }
    for (reach, side) in [
        (0.0, 32),
        (-3.0, 32),
        (f64::NAN, 32),
        (f64::INFINITY, 32),
        (50.0, 0),
    ] {
        assert_eq!(
            fit(Kind::Idle, reach, side).err(),
            Some(crate::error::FigureError::ScaleUnreal),
            "{reach} at {side}"
        );
    }
}

/// The shadow is under the figure when it stands on the ground and slides
/// away as it rises, which is the whole cue it exists to give.
#[test]
fn the_shadow_stays_put_and_fades_as_the_figure_rises() {
    let down = Reference::shadow(0.0, SCALE, AT).expect("a shadow");
    let up = Reference::shadow(30.0, SCALE, AT).expect("a shadow");
    assert!(mathf::fabs(down.x - AT.0) < 1e-9 && mathf::fabs(down.y - AT.1) < 1e-9);
    assert!(up.color.a < down.color.a, "a rising figure's shadow thins");
    assert!(
        mathf::hypot(up.x - AT.0, up.y - AT.1) > 1.0,
        "a rising figure's shadow slides away from the light"
    );
}
