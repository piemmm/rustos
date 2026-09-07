//! Unit tests for the pane flow's per-core grid: that a grid which wraps
//! spreads its cells evenly over the rows it needs, and that every cell of
//! it is the same size whichever row it lands in.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_controls::PressureKind;

use super::{
    cell_width, compile, grid_columns, BlockBody, CoreCell, ItemBody, PaneBlock, PaneHero,
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
                    badge: String::from("P"),
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
