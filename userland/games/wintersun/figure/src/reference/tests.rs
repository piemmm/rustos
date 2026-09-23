//! What the reference grid is, and the property that makes it one grid.

use tairix_util::mathf;

use alloc::vec;
use alloc::vec::Vec;

use super::{spec, Cell, Reference, Sampling, FACINGS, FIGURES, PHASES};
use crate::identity::{
    EarForm, EyeShape, FaceShape, Features, HairStyle, HornForm, Identity, Setting, TailForm,
};
use crate::motion::Kind;
use crate::rig::Placement;
use crate::species::Species;

const SCALE: f64 = 1.25;
const AT: (f64, f64) = (37.5, 92.25);

/// A figure's cells walk its motions, the phases and the headings exactly
/// once each: every motion for a species' reference, the walk alone for a
/// least or a most.
#[test]
fn every_figure_walks_its_own_cells_once_each() {
    for figure in &FIGURES {
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
    for figure in &FIGURES {
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
    for figure in &FIGURES {
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
    for figure in &FIGURES {
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
            assert!(
                planted.root().at.up <= 1e-12,
                "{} {cell:?} levitates by {}",
                figure.name,
                planted.root().at.up
            );
            assert!(!placement.is_empty());
        }
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
