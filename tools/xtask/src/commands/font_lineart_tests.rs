//! Tests for the synthesised line-art coverage.
//!
//! The properties asserted here are the reasons these glyphs are synthesised
//! at all: whole pixels, rules that reach their edges so neighbours join, and
//! blocks that tile with no seam. They are checked at several cell sizes, so
//! the generator stays correct if the atlas cell ever changes again.

use super::*;

/// Cell sizes to check every property at: the conventional console cell, the
/// square and tall shapes a different face would give, and a degenerate one.
const CELLS: [(u32, u32); 5] = [(8, 16), (15, 28), (16, 16), (10, 24), (3, 5)];

/// The coverage of `ch` in a `width`×`height` cell.
fn cell(ch: char, width: u32, height: u32) -> Vec<u8> {
    coverage(ch as u32, width, height).expect("a synthesised scalar")
}

fn at(pixels: &[u8], width: u32, x: u32, y: u32) -> u8 {
    pixels[(y * width + x) as usize]
}

/// A cell measurement as a count, for comparing against a number of pixels.
fn usize_of(value: u32) -> usize {
    usize::try_from(value).expect("a cell measurement fits a usize")
}

/// Rows that hold any ink.
fn inked_rows(pixels: &[u8], width: u32, height: u32) -> Vec<u32> {
    (0..height)
        .filter(|&y| (0..width).any(|x| at(pixels, width, x, y) != 0))
        .collect()
}

/// Columns that hold any ink.
fn inked_columns(pixels: &[u8], width: u32, height: u32) -> Vec<u32> {
    (0..width)
        .filter(|&x| (0..height).any(|y| at(pixels, width, x, y) != 0))
        .collect()
}

#[test]
fn only_the_two_tiling_ranges_are_synthesised() {
    assert!(coverage('─' as u32, 8, 16).is_some());
    assert!(coverage('╿' as u32, 8, 16).is_some(), "the last box scalar");
    assert!(coverage('▀' as u32, 8, 16).is_some());
    assert!(
        coverage('▟' as u32, 8, 16).is_some(),
        "the last block scalar"
    );
    // The scalars either side stay the face's job.
    assert!(coverage(0x24FF, 8, 16).is_none());
    assert!(coverage(0x25A0, 8, 16).is_none(), "geometric shapes");
    assert!(coverage('A' as u32, 8, 16).is_none());
}

#[test]
fn a_degenerate_cell_is_refused_rather_than_drawn() {
    assert!(coverage('─' as u32, 0, 16).is_none());
    assert!(coverage('█' as u32, 8, 0).is_none());
}

#[test]
fn every_synthesised_scalar_draws_whole_pixels() {
    // The whole point of drawing these rather than rasterising them: a pixel
    // is on or off, never a fraction that renders as a grey haze. The shades
    // are the deliberate exception, and are exactly their named fraction.
    for (width, height) in CELLS {
        for code in 0x2500..0x25A0 {
            let pixels = coverage(code, width, height).expect("a synthesised scalar");
            assert_eq!(pixels.len(), (width * height) as usize, "U+{code:04X}");
            let shade = matches!(code, 0x2591..=0x2593);
            for &value in &pixels {
                if shade {
                    assert!(matches!(value, 0 | 3 | 7 | 11), "U+{code:04X}: {value}");
                } else {
                    assert!(value == 0 || value == FULL, "U+{code:04X}: {value}");
                }
            }
        }
    }
}

#[test]
fn every_synthesised_scalar_draws_something() {
    // A blank cell would be a hole in the repertoire: the terminal would show
    // nothing where a rule or a block belongs.
    for (width, height) in CELLS {
        for code in 0x2500..0x25A0 {
            let pixels = coverage(code, width, height).expect("a synthesised scalar");
            assert!(
                pixels.iter().any(|&value| value != 0),
                "U+{code:04X} is blank at {width}×{height}"
            );
        }
    }
}

#[test]
fn a_rule_reaches_both_edges_so_neighbouring_cells_join() {
    // Two `─` cells side by side must read as one unbroken rule, which they do
    // only if each covers its own first and last column.
    for (width, height) in CELLS {
        for ch in ['─', '━', '═'] {
            let pixels = cell(ch, width, height);
            let columns = inked_columns(&pixels, width, height);
            assert_eq!(columns.first(), Some(&0), "{ch} at {width}×{height}");
            assert_eq!(
                columns.last(),
                Some(&(width - 1)),
                "{ch} at {width}×{height}"
            );
            assert_eq!(
                columns.len(),
                usize_of(width),
                "{ch} at {width}×{height} has a gap"
            );
        }
        for ch in ['│', '┃', '║'] {
            let pixels = cell(ch, width, height);
            let rows = inked_rows(&pixels, width, height);
            assert_eq!(rows.first(), Some(&0), "{ch} at {width}×{height}");
            assert_eq!(rows.last(), Some(&(height - 1)), "{ch} at {width}×{height}");
            assert_eq!(
                rows.len(),
                usize_of(height),
                "{ch} at {width}×{height} has a gap"
            );
        }
    }
}

#[test]
fn a_half_rule_stops_at_the_centre() {
    // `╶` is the right half only: it must not reach the left edge, or a lone
    // stub would read as a full rule.
    for (width, height) in CELLS {
        let right = cell('╶', width, height);
        let columns = inked_columns(&right, width, height);
        assert_eq!(columns.last(), Some(&(width - 1)), "{width}×{height}");
        assert!(columns.first() > Some(&0) || width <= 2, "{width}×{height}");
        let left = cell('╴', width, height);
        assert_eq!(
            inked_columns(&left, width, height).first(),
            Some(&0),
            "{width}×{height}"
        );
    }
}

#[test]
fn a_corner_joins_exactly_the_two_edges_it_names() {
    // `┌` opens down and right, so it reaches the bottom and right edges and
    // touches neither the top nor the left.
    for (width, height) in CELLS {
        let pixels = cell('┌', width, height);
        let touches_left = (0..height).any(|y| at(&pixels, width, 0, y) != 0);
        let touches_top = (0..width).any(|x| at(&pixels, width, x, 0) != 0);
        let touches_right = (0..height).any(|y| at(&pixels, width, width - 1, y) != 0);
        let touches_bottom = (0..width).any(|x| at(&pixels, width, x, height - 1) != 0);
        assert!(
            !touches_left && !touches_top && touches_right && touches_bottom,
            "┌ at {width}×{height}"
        );
    }
}

#[test]
fn a_cross_reaches_all_four_edges() {
    for (width, height) in CELLS {
        for ch in ['┼', '╋', '╬'] {
            let pixels = cell(ch, width, height);
            assert!(
                (0..height).any(|y| at(&pixels, width, 0, y) != 0),
                "{ch} left"
            );
            assert!(
                (0..height).any(|y| at(&pixels, width, width - 1, y) != 0),
                "{ch} right"
            );
            assert!(
                (0..width).any(|x| at(&pixels, width, x, 0) != 0),
                "{ch} top"
            );
            assert!(
                (0..width).any(|x| at(&pixels, width, x, height - 1) != 0),
                "{ch} bottom"
            );
        }
    }
}

#[test]
fn a_heavy_rule_is_thicker_than_a_light_one_where_the_cell_allows() {
    for (width, height) in CELLS {
        let light = inked_rows(&cell('─', width, height), width, height).len();
        let heavy = inked_rows(&cell('━', width, height), width, height).len();
        assert!(heavy > light || height < 2, "{width}×{height}");
    }
}

#[test]
fn a_double_rule_is_two_rails_with_a_gap_between_them() {
    // `═` must read as two lines, not one thick one: an uninked row between
    // two inked ones is what makes it double.
    for (width, height) in CELLS.into_iter().filter(|&(_, h)| h >= 5) {
        let pixels = cell('═', width, height);
        let rows = inked_rows(&pixels, width, height);
        assert!(rows.len() >= 2, "{width}×{height}");
        let gap = (rows[0] + 1..*rows.last().expect("an inked row"))
            .any(|y| (0..width).all(|x| at(&pixels, width, x, y) == 0));
        assert!(gap, "═ at {width}×{height} has no gap between its rails");
    }
}

#[test]
fn a_double_corner_leaves_its_inner_notch_open() {
    // The classic double corner: the outer rails turn at the corner and the
    // inner ones stop short, leaving the small square notch open. Without it
    // `╔` would render as a solid quarter-block.
    let (width, height) = (8, 16);
    let pixels = cell('╔', width, height);
    let strokes = Strokes::for_cell(width, height);
    let vertical = centred(width, strokes.double);
    let horizontal = centred(height, strokes.double);
    assert_eq!(
        at(&pixels, width, vertical + 1, horizontal + 1),
        0,
        "the notch inside ╔ must be open"
    );
    // …while both rails are present along the arms.
    assert_ne!(
        at(&pixels, width, vertical, height - 1),
        0,
        "outer rail down"
    );
    assert_ne!(
        at(&pixels, width, vertical + 2, height - 1),
        0,
        "inner rail down"
    );
    assert_ne!(
        at(&pixels, width, width - 1, horizontal),
        0,
        "outer rail right"
    );
    assert_ne!(
        at(&pixels, width, width - 1, horizontal + 2),
        0,
        "inner rail right"
    );
}

#[test]
fn a_dashed_rule_is_broken_and_a_plain_one_is_not() {
    for (width, height) in CELLS.into_iter().filter(|&(w, _)| w >= 8) {
        for (ch, pieces) in [('╌', 2u32), ('┄', 3), ('┈', 4)] {
            let pixels = cell(ch, width, height);
            let inked = inked_columns(&pixels, width, height);
            assert!(
                inked.len() < usize_of(width),
                "{ch} at {width}×{height} is not broken"
            );
            assert!(!inked.is_empty(), "{ch} at {width}×{height} drew nothing");
            let gaps = (1..width).filter(|x| !inked.contains(x) && inked.contains(&(x - 1)));
            assert!(
                gaps.count() >= usize_of(pieces - 1),
                "{ch} has too few gaps"
            );
        }
    }
}

#[test]
fn the_full_block_covers_the_cell_so_a_filled_area_has_no_seam() {
    // The defect that motivated synthesising these: an outline `█` leaves its
    // outermost rows partly covered, so a filled region shows a lighter band
    // at every cell boundary.
    for (width, height) in CELLS {
        let pixels = cell('█', width, height);
        assert!(
            pixels.iter().all(|&value| value == FULL),
            "█ at {width}×{height} does not fill its cell"
        );
    }
}

#[test]
fn complementary_halves_tile_into_a_full_block() {
    // Upper plus lower half, and left plus right half, must together cover the
    // cell exactly once — no overlap and no uncovered row or column.
    for (width, height) in CELLS {
        for (first, second) in [('▀', '▄'), ('▌', '▐')] {
            let a = cell(first, width, height);
            let b = cell(second, width, height);
            for (index, (&one, &other)) in a.iter().zip(b.iter()).enumerate() {
                assert_ne!(
                    one != 0,
                    other != 0,
                    "{first}{second} at {width}×{height}: pixel {index} is covered \
                     by both halves or by neither"
                );
            }
        }
    }
}

#[test]
fn the_eighth_blocks_grow_one_step_at_a_time() {
    // U+2581..U+2588 fill from the bottom in eight steps, each at least as
    // tall as the last and the eighth one the whole cell.
    let (width, height) = (8, 16);
    let mut previous = 0;
    for code in 0x2581..=0x2588u32 {
        let pixels = coverage(code, width, height).expect("a block scalar");
        let rows = inked_rows(&pixels, width, height).len();
        assert!(rows >= previous, "U+{code:04X} shrank");
        assert_eq!(
            at(&pixels, width, 0, height - 1),
            FULL,
            "U+{code:04X} must sit on the bottom edge"
        );
        previous = rows;
    }
    assert_eq!(
        previous, height as usize,
        "the eighth step is the full block"
    );
}

#[test]
fn the_quadrants_tile_into_the_blocks_they_compose() {
    // `▘`+`▝`+`▖`+`▗` is `█`, and the two-corner characters are the unions
    // their names claim — the property that lets them be used as a 2×2 pixel
    // font without gaps.
    let (width, height) = (8, 16);
    let union = |chars: &[char]| {
        let mut out = vec![0u8; (width * height) as usize];
        for &ch in chars {
            for (slot, value) in out.iter_mut().zip(cell(ch, width, height)) {
                *slot = (*slot).max(value);
            }
        }
        out
    };
    assert_eq!(union(&['▘', '▝', '▖', '▗']), cell('█', width, height));
    assert_eq!(union(&['▘', '▗']), cell('▚', width, height));
    assert_eq!(union(&['▝', '▖']), cell('▞', width, height));
    assert_eq!(union(&['▘', '▖', '▗']), cell('▙', width, height));
    assert_eq!(union(&['▘', '▝', '▖']), cell('▛', width, height));
    assert_eq!(union(&['▘', '▝', '▗']), cell('▜', width, height));
    assert_eq!(union(&['▝', '▖', '▗']), cell('▟', width, height));
}

#[test]
fn the_shades_are_uniform_and_ordered() {
    // A shade tints a whole cell evenly, so two neighbouring shaded cells are
    // one tone; and the three are ordered light to dark.
    for (width, height) in CELLS {
        let tone = |ch: char| {
            let pixels = cell(ch, width, height);
            let first = pixels[0];
            assert!(
                pixels.iter().all(|&value| value == first),
                "{ch} is not uniform"
            );
            first
        };
        let (light, medium, dark) = (tone('░'), tone('▒'), tone('▓'));
        assert!(0 < light && light < medium && medium < dark && dark < FULL);
    }
}

#[test]
fn a_diagonal_runs_corner_to_corner() {
    for (width, height) in CELLS {
        let falling = cell('╲', width, height);
        assert_ne!(at(&falling, width, 0, 0), 0, "{width}×{height} top left");
        assert_ne!(
            at(&falling, width, width - 1, height - 1),
            0,
            "{width}×{height} bottom right"
        );
        let rising = cell('╱', width, height);
        assert_ne!(
            at(&rising, width, width - 1, 0),
            0,
            "{width}×{height} top right"
        );
        assert_ne!(
            at(&rising, width, 0, height - 1),
            0,
            "{width}×{height} bottom left"
        );
        // The cross is exactly the two of them together.
        let cross = cell('╳', width, height);
        for (index, value) in cross.iter().enumerate() {
            assert_eq!(*value, falling[index].max(rising[index]), "╳ pixel {index}");
        }
    }
}

/// Whether every inked pixel is reachable from every other by steps to a
/// touching pixel, counting diagonals — a rule that is one continuous line.
fn ink_is_connected(pixels: &[u8], width: u32, height: u32) -> bool {
    let inked: Vec<(u32, u32)> = (0..height)
        .flat_map(|y| (0..width).map(move |x| (x, y)))
        .filter(|&(x, y)| at(pixels, width, x, y) != 0)
        .collect();
    let Some(&first) = inked.first() else {
        return false;
    };
    let mut seen = vec![first];
    let mut frontier = vec![first];
    while let Some((x, y)) = frontier.pop() {
        for (nx, ny) in inked.iter().copied() {
            let touching = nx.abs_diff(x) <= 1 && ny.abs_diff(y) <= 1;
            if touching && !seen.contains(&(nx, ny)) {
                seen.push((nx, ny));
                frontier.push((nx, ny));
            }
        }
    }
    seen.len() == inked.len()
}

#[test]
fn a_corner_is_one_unbroken_line_however_it_turns() {
    // Mitred or rounded, a corner is a single stroke: a pixel the turn misses
    // shows as a nick in the border.
    for (width, height) in CELLS {
        for ch in ['┌', '┐', '└', '┘', '╭', '╮', '╯', '╰'] {
            let pixels = cell(ch, width, height);
            assert!(
                ink_is_connected(&pixels, width, height),
                "{ch} at {width}×{height} is broken into pieces"
            );
        }
    }
}

#[test]
fn an_arc_is_rounded_yet_still_joins_its_edges() {
    // Each arc opens the same two ways as its mitred twin, so it must reach
    // the same two edges; being rounded, it must not paint the sharp corner.
    for (width, height) in CELLS.into_iter().filter(|&(w, h)| w >= 8 && h >= 8) {
        for (rounded, mitred, right_edge, bottom_edge) in [
            ('╭', '┌', true, true),
            ('╮', '┐', false, true),
            ('╯', '┘', false, false),
            ('╰', '└', true, false),
        ] {
            let arc = cell(rounded, width, height);
            let column = if right_edge { width - 1 } else { 0 };
            let row = if bottom_edge { height - 1 } else { 0 };
            assert!(
                (0..width).any(|x| at(&arc, width, x, row) != 0),
                "{rounded} at {width}×{height} must reach its horizontal edge"
            );
            assert!(
                (0..height).any(|y| at(&arc, width, column, y) != 0),
                "{rounded} at {width}×{height} must reach its vertical edge"
            );
            let ink = |pixels: &[u8]| pixels.iter().filter(|&&value| value != 0).count();
            assert!(
                ink(&arc) < ink(&cell(mitred, width, height)),
                "{rounded} at {width}×{height} is not rounded off"
            );
        }
    }
}
