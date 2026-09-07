//! Unit tests for the pane flow's per-core grid: that a grid which wraps
//! spreads its cells evenly over the rows it needs, that every cell of it is
//! the same size whichever row it lands in, and that a drawn cell wears the
//! rim and the class-toned badge the storyboards show
//! (`plans/switchboard/02-cpu.png`).

use alloc::vec::Vec;

use tairix_abi::sysinfo::CpuCoreClass;
use tairix_controls::PressureKind;
use tairix_geometry::{Rect, Scale};
use tairix_icon::NoArtwork;
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Rgba, Theme};

use super::{
    cell_width, compile, grid_columns, render, BlockBody, CoreCell, ItemBody, PaneBlock, PaneHero,
    PaneWindow,
};
use crate::view::reading::Reading;

/// A grid of `count` cores, each with a reading of its own.
fn cores(count: usize) -> PaneBlock {
    PaneBlock::full(
        "PER-CORE BUSY",
        BlockBody::Cores(
            (0..count)
                .map(|i| CoreCell {
                    label: alloc::format!("core {i}"),
                    class: CpuCoreClass::Performance,
                    busy: Reading::measured("41%"),
                    clock: Reading::measured("3.9 GHz"),
                    trend: alloc::vec![300, 500, 400],
                })
                .collect(),
        ),
    )
}

/// The grid rows a pane of `count` cores compiles to, `most` cells wide:
/// each row's cell count beside the column count it declares.
fn rows(count: usize, most: u32) -> Vec<(usize, u32)> {
    let hero = PaneHero::facts(Reading::measured("18%"), "% busy");
    compile(&hero, false, &[cores(count)], PressureKind::Cpu, most)
        .iter()
        .filter_map(|item| match &item.body {
            ItemBody::Cells { cells, columns } => Some((cells.len(), *columns)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_grid_that_fits_one_row_draws_one_row() {
    assert_eq!(rows(4, 6), alloc::vec![(4, 4)]);
    assert_eq!(rows(6, 6), alloc::vec![(6, 6)]);
}

#[test]
fn a_wrapping_grid_spreads_its_cells_evenly_over_the_rows_it_needs() {
    // Four cores in a pane three cells wide are two rows of two, never
    // three and a lone straggler stretched across the width beneath them.
    assert_eq!(rows(4, 3), alloc::vec![(2, 2), (2, 2)]);
    // Twelve in a pane six wide are two full rows.
    assert_eq!(rows(12, 6), alloc::vec![(6, 6), (6, 6)]);
    // Seven in a pane six wide are four and three, not six and one.
    assert_eq!(rows(7, 6), alloc::vec![(4, 4), (3, 4)]);
}

#[test]
fn every_row_of_a_grid_declares_the_grid_s_own_column_count() {
    // A row that cannot be filled still divides the grid's columns, so its
    // cells are the size of every other row's rather than stretching.
    for (cells, columns) in rows(5, 3) {
        assert_eq!(columns, 3, "{cells} cells must divide the grid's columns");
    }
    assert_eq!(rows(5, 3), alloc::vec![(3, 3), (2, 3)]);
}

#[test]
fn a_single_core_machine_draws_one_cell() {
    assert_eq!(rows(1, 6), alloc::vec![(1, 1)]);
}

#[test]
fn balancing_costs_no_extra_row_and_never_exceeds_the_width() {
    // Spreading the cells evenly must not push the grid onto another row,
    // and must never ask for more columns than the pane can seat.
    for count in 1..64usize {
        for most in 1..=6u32 {
            let columns = grid_columns(count, most);
            assert!(columns >= 1, "{count} in {most}: a grid has a column");
            assert!(columns <= most, "{count} in {most}: wider than the pane");
            assert!(
                usize::try_from(columns).unwrap_or(1) <= count,
                "{count} in {most}: more columns than cells"
            );
            let widest = usize::try_from(most).unwrap_or(1);
            assert_eq!(
                count.div_ceil(usize::try_from(columns).unwrap_or(1)),
                count.div_ceil(widest),
                "{count} in {most}: balancing added a row"
            );
        }
    }
}

#[test]
fn a_cells_width_comes_from_the_grid_rather_than_its_row() {
    // The width is a function of the pane and the grid's columns alone, so
    // the final row of a wrapped grid cannot widen its cells. A grid's
    // cells fit the pane with its gaps, and a narrower grid has wider
    // cells.
    let gap = 4;
    let width = 600;
    for columns in 1..=6u32 {
        let cell = cell_width(width, columns, gap);
        let spanned = cell
            .saturating_mul(columns)
            .saturating_add(gap.saturating_mul(columns.saturating_sub(1)));
        assert!(spanned <= width, "{columns} columns overflow the pane");
        assert!(
            spanned.saturating_add(columns) > width,
            "{columns} columns leave a whole cell's slack"
        );
    }
    assert!(cell_width(width, 3, gap) < cell_width(width, 2, gap));
    // A pane too narrow for its columns yields no cell at all, which the
    // paint reads as "draw nothing" rather than dividing by nought.
    assert_eq!(cell_width(8, 6, gap), 0);
}

// --- A drawn cell -----------------------------------------------------------

const PANE_W: u32 = 640;
const PANE_H: u32 = 320;

fn premul(rgba: Rgba) -> Pixel {
    Color::from(rgba).premultiply()
}

/// The grid of `count` cores, each with `class`, painted into one surface.
fn cell_surface(count: usize, class: CpuCoreClass, theme: &Theme) -> Surface {
    let hero = PaneHero::facts(Reading::measured("18%"), "% busy");
    let mut block = cores(count);
    if let BlockBody::Cores(cells) = &mut block.body {
        for cell in cells.iter_mut() {
            cell.class = class;
        }
    }
    let items = compile(&hero, false, &[block], PressureKind::Cpu, 6);
    let mut surface = Surface::new(PANE_W, PANE_H).expect("surface");
    render(
        &mut surface,
        &items,
        PaneWindow {
            primary: Rect::new(0, 0, PANE_W, PANE_H),
            start: 0,
            scale: Scale::ONE,
            theme,
            font: tairix_font::BitmapFont::console(),
        },
        &mut NoArtwork,
    );
    surface
}

fn has_pixel(surface: &Surface, want: Pixel) -> bool {
    surface.pixels().contains(&want)
}

#[test]
fn a_cell_draws_its_own_rim() {
    // Before the rim landed a cell was three overlaid controls with no edge at
    // all, so a dozen cores read as one undivided field of figures.
    for theme in [Theme::dark(), Theme::light()] {
        let surface = cell_surface(4, CpuCoreClass::Performance, &theme);
        assert!(
            has_pixel(&surface, premul(theme.palette().rim)),
            "{}",
            theme.name()
        );
    }
}

#[test]
fn a_cells_badge_is_toned_by_the_cores_class() {
    let theme = Theme::dark();
    let compute = premul(theme.palette().cpu_pressure);
    let healthy = premul(theme.palette().success);

    // An efficiency core's badge reads as the healthy tone; a throughput
    // core's does not, so a heterogeneous machine's two kinds separate.
    let efficiency = cell_surface(4, CpuCoreClass::Efficiency, &theme);
    assert!(has_pixel(&efficiency, healthy), "an E badge is toned");

    let performance = cell_surface(4, CpuCoreClass::Performance, &theme);
    assert!(
        !has_pixel(&performance, healthy),
        "a P badge must not wear the efficiency tone"
    );
    // The compute tone is the trace's own colour too, so its presence proves
    // nothing on its own; what matters is that the two classes differ.
    assert!(has_pixel(&performance, compute));
    assert_ne!(efficiency.pixels(), performance.pixels());
}

#[test]
fn a_cells_readings_stay_inside_its_own_rim() {
    // The rim is drawn first and the readings over it, so content that spanned
    // the whole cell would erase the left and right edges. The rim must survive
    // on every side of a cell.
    let theme = Theme::dark();
    let rim = premul(theme.palette().rim);
    let surface = cell_surface(1, CpuCoreClass::Performance, &theme);
    let column_has = |x: u32| (0..PANE_H).any(|y| surface.get(x, y) == Some(rim));
    let left = (0..PANE_W).find(|&x| column_has(x)).expect("a left edge");
    let right = (0..PANE_W).rfind(|&x| column_has(x)).expect("a right edge");
    assert!(right > left, "left {left} right {right}");
}

#[test]
fn a_consumer_row_asks_the_cache_for_the_launching_applications_picture() {
    /// An artwork lookup recording what it was asked for, answering none.
    #[derive(Default)]
    struct Recording {
        asked: Vec<(tairix_icon::IconKind, u32)>,
    }
    impl tairix_icon::IconArtwork for Recording {
        fn artwork(
            &mut self,
            request: tairix_icon::IconRequest<'_>,
            side: u32,
        ) -> Option<tairix_icon::IconPicture<'_>> {
            self.asked.push((request.icon_kind(), side));
            None
        }
    }

    let theme = Theme::dark();
    let hero = PaneHero::facts(Reading::measured("18%"), "% busy");
    let block = PaneBlock::half(
        "TOP CONSUMERS",
        BlockBody::Consumers(alloc::vec![
            super::ConsumerRow {
                name: alloc::string::String::from("terminal"),
                bundle: Some(alloc::string::String::from("/Apps/Terminal.app")),
                amount: alloc::string::String::from("9.7%"),
                share: 970,
            },
            super::ConsumerRow {
                name: alloc::string::String::from("init"),
                bundle: None,
                amount: alloc::string::String::from("0.1%"),
                share: 10,
            },
        ]),
    );
    let items = compile(&hero, false, &[block], PressureKind::Cpu, 6);
    let mut surface = Surface::new(PANE_W, PANE_H).expect("surface");
    let mut artwork = Recording::default();
    render(
        &mut surface,
        &items,
        PaneWindow {
            primary: Rect::new(0, 0, PANE_W, PANE_H),
            start: 0,
            scale: Scale::ONE,
            theme: &theme,
            font: tairix_font::BitmapFont::console(),
        },
        &mut artwork,
    );

    let kinds: Vec<tairix_icon::IconKind> = artwork.asked.iter().map(|&(kind, _)| kind).collect();
    assert!(
        kinds.contains(&tairix_icon::IconKind::AppBundle),
        "the launched application's row asks for its own picture: {kinds:?}"
    );
    assert!(
        kinds.contains(&tairix_icon::IconKind::Executable),
        "a process nothing attests asks for the executable class: {kinds:?}"
    );
    assert!(
        artwork.asked.iter().all(|&(_, side)| side > 0),
        "and each at the side its tile actually draws: {:?}",
        artwork.asked
    );
}
