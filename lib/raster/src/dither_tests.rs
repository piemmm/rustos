//! Unit tests for the ordered-dither rounding bias.

use super::{DitherRow, MATRIX};
use crate::color::{div255, div255_biased, ROUND_NEAREST};

/// Every bias of one tile, in reading order.
fn tile() -> [u32; 64] {
    let mut all = [0u32; 64];
    let mut cells = all.iter_mut();
    for y in 0..8u32 {
        let row = DitherRow::at(y);
        for x in 0..8u32 {
            if let Some(cell) = cells.next() {
                *cell = row.bias(x);
            }
        }
    }
    all
}

#[test]
fn every_bias_lands_inside_one_rounding_level() {
    // A bias of a whole level would not choose where in the level to round —
    // it would shift the value onto the next one.
    for (cell, bias) in tile().into_iter().enumerate() {
        assert!(bias < 255, "bias {bias} at cell {cell} is a whole level");
    }
}

#[test]
fn the_matrix_ranks_every_cell_of_the_tile_exactly_once() {
    let mut seen = [false; 64];
    for row in MATRIX {
        for cell in row {
            let rank = usize::from(cell);
            assert!(!seen[rank], "rank {cell} appears twice");
            seen[rank] = true;
        }
    }
    assert!(seen.iter().all(|seen| *seen), "a rank is missing");
}

#[test]
fn the_tile_spends_all_64_sub_levels_and_averages_to_nearest_rounding() {
    let mut biases = tile();
    let total: u32 = biases.iter().sum();
    // The mean bias is nearest rounding itself, so a dithered paint has the
    // same mean as an undithered one: the dither trades a systematic rounding
    // error for a spatial one, it neither lightens nor darkens.
    assert_eq!(total, ROUND_NEAREST * 64);

    biases.sort_unstable();
    assert!(
        biases.windows(2).all(|pair| pair[0] != pair[1]),
        "two cells round alike"
    );
}

#[test]
fn the_pattern_tiles_the_surface_and_is_the_same_every_time() {
    for y in 0..8u32 {
        let row = DitherRow::at(y);
        for x in 0..8u32 {
            let here = row.bias(x);
            assert_eq!(here, DitherRow::at(y).bias(x), "not a pure function");
            assert_eq!(here, row.bias(x + 8), "does not tile across");
            assert_eq!(here, DitherRow::at(y + 8).bias(x), "does not tile down");
            assert_eq!(here, DitherRow::at(y + 8192).bias(x + 8192), "drifts");
        }
    }
}

#[test]
fn the_no_dither_row_rounds_every_column_to_nearest() {
    for x in 0..16u32 {
        assert_eq!(DitherRow::NEAREST.bias(x), ROUND_NEAREST);
    }
}

#[test]
fn rounding_at_the_neutral_bias_is_exactly_the_nearest_integer() {
    // The whole scheme rests on this: `div255` *is* the biased divide at
    // `ROUND_NEAREST`, so every operator that takes a bias is the same
    // arithmetic as the plain one and no existing pixel moves.
    for value in 0..=65025u32 {
        let exact = u8::try_from((2 * value + 255) / 510).unwrap_or(u8::MAX);
        assert_eq!(div255_biased(value, ROUND_NEAREST), exact, "at {value}");
        assert_eq!(div255(value), exact, "at {value}");
    }
}

#[test]
fn a_bias_moves_a_quotient_by_at_most_one_level() {
    // A dithered pixel is never more than a level from the undithered one,
    // whatever the bias, so a dither can only ever choose between the two
    // levels the exact value falls between.
    for value in (0..=65025u32).step_by(7) {
        let nearest = u32::from(div255(value));
        for bias in [0, 1, 64, ROUND_NEAREST, 200, 254] {
            let biased = u32::from(div255_biased(value, bias));
            assert!(biased.abs_diff(nearest) <= 1, "{value} at bias {bias}");
        }
    }
}

#[test]
fn a_tile_is_the_eight_columns_from_its_own_start() {
    // A span composite reads the tile by each pixel's position within the
    // span instead of by its surface column, so the two must agree for every
    // phase or a run would dither at the wrong place on the surface.
    for y in [0u32, 1, 5, 7, 8, 4096] {
        let row = DitherRow::at(y);
        for first_x in 0..24u32 {
            let tile = row.tile_at(first_x);
            for (lane, bias) in tile.into_iter().enumerate() {
                let column = first_x + u32::try_from(lane).expect("a lane index fits");
                assert_eq!(bias, row.bias(column), "row {y}, column {column}");
            }
        }
    }
}

#[test]
fn the_no_dither_tile_rounds_every_lane_to_nearest() {
    assert_eq!(
        DitherRow::NEAREST.tile_at(3),
        [ROUND_NEAREST; DitherRow::PERIOD]
    );
}
