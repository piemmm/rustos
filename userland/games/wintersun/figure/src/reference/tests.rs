//! What the reference grid is, and the property that makes it one grid.

use tairix_util::mathf;

use super::{Cell, Reference, FACINGS, PHASES};
use crate::motion::Kind;
use crate::rig::Placement;

const SCALE: f64 = 1.25;
const AT: (f64, f64) = (37.5, 92.25);

/// Every index names a cell, no index names two, and the walk covers every
/// motion, phase and heading exactly once.
#[test]
fn the_grid_is_a_complete_enumeration() {
    assert_eq!(Cell::COUNT, Kind::ALL.len() * PHASES * FACINGS.len());
    assert_eq!(Cell::at(Cell::COUNT), None);
    let mut seen = [false; Cell::COUNT];
    for index in 0..Cell::COUNT {
        let cell = Cell::at(index).expect("a cell");
        let slot = cell.kind.index() * PHASES * FACINGS.len()
            + cell.step * FACINGS.len()
            + FACINGS
                .iter()
                .position(|f| *f == cell.facing)
                .expect("a listed heading");
        assert!(
            !seen[slot],
            "cell {index} repeats a motion, phase and heading"
        );
        seen[slot] = true;
    }
    assert!(seen.iter().all(|s| *s));
}

/// The heading picks which way the figure is seen, never what it is doing —
/// otherwise a contact sheet's four columns would be four animations.
#[test]
fn the_pose_does_not_depend_on_the_heading() {
    let reference = Reference::new().expect("the shipped figure");
    let mut placement = Placement::new();
    for kind in Kind::ALL {
        for step in 0..PHASES {
            let mut root = None;
            for facing in FACINGS {
                let cell = Cell { kind, step, facing };
                let planted = reference
                    .place(cell, SCALE, AT, &mut placement)
                    .expect("it places");
                match root {
                    None => root = Some(planted),
                    Some(first) => assert_eq!(
                        first.pose(),
                        planted.pose(),
                        "{} at {step} poses differently facing {facing:?}",
                        kind.name()
                    ),
                }
            }
        }
    }
}

/// A sheet is a drawing of the clip, so on the level ground the grid stands
/// on the planting solve must put the planted foot down and change nothing
/// else.
#[test]
fn every_cell_stands_on_the_ground_it_was_given() {
    let reference = Reference::new().expect("the shipped figure");
    let mut placement = Placement::new();
    for index in 0..Cell::COUNT {
        let cell = Cell::at(index).expect("a cell");
        let planted = reference
            .place(cell, SCALE, AT, &mut placement)
            .expect("it places");
        assert!(
            planted.worst_miss() < 1e-6,
            "cell {index} missed by {}",
            planted.worst_miss()
        );
        assert!(
            planted.root().at.up <= 1e-12,
            "cell {index} levitates by {}",
            planted.root().at.up
        );
        assert!(!placement.is_empty());
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
