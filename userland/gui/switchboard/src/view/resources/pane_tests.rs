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
    // the final row of a wrapped grid cannot widen its cells. The slots abut
    // and each cell's own plate margin makes the gap, so they divide the pane
    // between them and a narrower grid has wider cells.
    let width = 600u32;
    for columns in 1..=6u32 {
        let cell = cell_width(width, columns);
        let spanned = cell.saturating_mul(columns);
        assert!(spanned <= width, "{columns} columns overflow the pane");
        assert!(
            spanned.saturating_add(columns) > width,
            "{columns} columns leave a whole cell's slack"
        );
    }
    assert!(cell_width(width, 3) < cell_width(width, 2));
    // A pane too narrow for its columns yields no cell at all, which the
    // paint reads as "draw nothing" rather than dividing by nought.
    assert_eq!(cell_width(8, 6), 1);
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
            home: None,
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
            home: None,
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

/// The hero is the one figure the pane is built around, so it is set in the
/// display role rather than the panel-heading role a block title takes.
///
/// Asserted through the height the tile measures — the role's own line — so
/// the test observes what a reader sees rather than a field.
#[test]
fn the_hero_is_set_in_the_display_role() {
    let theme = Theme::dark();
    let hero = PaneHero::facts(Reading::measured("18"), "% busy");
    let items = compile(&hero, false, &[], PressureKind::Cpu, 6);
    let tile = hero_tile(&items);

    let reference = |role| {
        tairix_controls::MetricTile::new(
            alloc::string::String::new(),
            alloc::string::String::from("18"),
            PressureKind::Cpu,
        )
        .with_layout(tairix_controls::MetricLayout::Stacked)
        .with_value_role(role)
        .with_unit(alloc::string::String::from("% busy"))
        .unplated()
        .measured_height(Scale::ONE, &theme)
    };
    assert_eq!(
        tile.measured_height(Scale::ONE, &theme),
        reference(tairix_theme::TextRole::Display)
    );
    assert!(
        reference(tairix_theme::TextRole::Display) > reference(tairix_theme::TextRole::Heading),
        "the display rung is not taller than the heading rung this replaced"
    );
}

/// The taller figure must not cost the hero a context line: the rows it claims
/// still seat the tile and the line the flow carries beneath it.
#[test]
fn the_display_hero_still_seats_both_context_lines() {
    let theme = Theme::dark();
    let hero = PaneHero::facts(Reading::measured("18"), "% busy").with_context(alloc::vec![
        alloc::string::String::from("2.2 of 12 cores-equivalent"),
        alloc::string::String::from("Load average 1.24 · 1.09 · 0.92"),
    ]);
    let items = compile(&hero, false, &[], PressureKind::Cpu, 6);
    let hero_item = items
        .iter()
        .find(|item| matches!(item.body, ItemBody::Hero { .. }))
        .expect("the pane's hero");
    let ItemBody::Hero { tile, context, .. } = &hero_item.body else {
        unreachable!("matched above")
    };

    // The tile carries the first line as its own detail; the flow draws the
    // rest under it.
    assert_eq!(context.len(), 1);
    let font = tairix_font::BitmapFont::console();
    let pitch = super::pitch(Scale::ONE, &theme);
    let needed = tile.measured_height(Scale::ONE, &theme) + font.line_height();
    assert!(
        needed <= hero_item.rows * pitch,
        "the display-role hero does not seat its context in {} rows: {needed} > {}",
        hero_item.rows,
        hero_item.rows * pitch
    );
}

/// The axis row under a hero's trace was drawn flush to the plate's lower
/// border: the flow insets a plated item's top but not its bottom, which is
/// right for a row inside a plate (that inset is its leading) and wrong for
/// the hero, which spans the whole plate.
#[test]
fn a_hero_closes_the_plate_edge_below_its_axis() {
    let theme = Theme::dark();
    let inset = crate::view::block::content_inset(Scale::ONE, &theme);
    let band = Rect::new(0, 0, 400, 160);
    let rect = super::hero_rect(band, Scale::ONE, &theme).expect("the hero draws");

    assert_eq!(
        band.bottom() - rect.bottom(),
        i32::try_from(inset).unwrap_or(0),
        "the hero must leave the plate's own margin below its axis"
    );
    assert_eq!(
        rect.top(),
        band.top(),
        "the top inset is the flow's, not ours"
    );
}

/// A band with no room left for the margin draws nothing rather than
/// wrapping its height around zero.
#[test]
fn a_hero_with_no_room_for_its_margin_draws_nothing() {
    let theme = Theme::dark();
    let inset = crate::view::block::content_inset(Scale::ONE, &theme);
    let band = Rect::new(0, 0, 400, inset);
    assert!(super::hero_rect(band, Scale::ONE, &theme).is_none());
}

/// The hero's built tile, from the flow the pane compiled to.
fn hero_tile(items: &[super::PaneItem]) -> &tairix_controls::MetricTile {
    items
        .iter()
        .find_map(|item| match &item.body {
            ItemBody::Hero { tile, .. } => Some(tile),
            _ => None,
        })
        .expect("the pane's hero")
}

/// The axis row states the box's span at both ends, so the trace reads as a
/// *window* rather than a shape. The span is derived from the sampler's own
/// cadence, so it cannot claim a minute over a two-minute window.
#[test]
fn a_traces_axis_row_states_its_own_span_and_that_its_edge_is_now() {
    let label = super::trace_window_label();
    let seconds = tairix_controls::MAX_CHART_SAMPLES as u64
        * (crate::schedule::SAMPLE_PERIOD_NS / 1_000_000_000);
    assert_eq!(label, alloc::format!("-{seconds} s"));
    assert!(
        seconds > 60,
        "the window is {seconds} s, so a hard-coded -60 s would be a fabricated span"
    );
    assert_eq!(super::AXIS_NOW, "now");
}

/// A block's plate closes below its last reading rather than through it: the
/// rows are inset from the top of the band, so a plate exactly as tall as its
/// content ran the last row over its own rim — which is what put the memory
/// hero's share bar outside its plate.
#[test]
fn a_plated_block_claims_room_below_its_last_row() {
    let theme = Theme::dark();
    let hero = PaneHero::facts(Reading::measured("8.5"), "/ 16.0 GiB");
    let block = super::PaneBlock::full(
        "MEMORY",
        BlockBody::Facts(alloc::vec![crate::view::reading::ReadingFact::text(
            "Swap", "none"
        )]),
    );
    let items = compile(
        &hero,
        false,
        core::slice::from_ref(&block),
        PressureKind::Memory,
        6,
    );
    let plate = items
        .iter()
        .find(|item| matches!(item.body, super::ItemBody::Plate) && item.row > 0)
        .expect("the block plates itself");
    let last = items
        .iter()
        .filter(|item| item.plated && item.row >= plate.row)
        .map(|item| item.row + item.rows)
        .max()
        .expect("the block has rows");
    assert!(
        plate.row + plate.rows > last,
        "the plate ends level with its last row, so that row runs over its rim"
    );
    let pad = crate::view::block::content_inset(Scale::ONE, &theme);
    assert!(pad > 0, "a plate with no padding has nothing to overrun");
}
