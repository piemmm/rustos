//! Unit tests for the grid fitter, over synthetic outlines whose exact
//! geometry is known and over the committed console face.

use core::cmp::Ordering;

use alloc::vec::Vec;

use crate::engine::Segment;
use crate::gridfit::{fit, AlignZones, Axes, Zone};
use crate::mathf;
use crate::tests::asset;
use crate::{CellGeometry, Face, ATLAS_EM_PX};

/// One outline segment.
fn seg(x0: f64, y0: f64, x1: f64, y1: f64) -> Segment {
    Segment { x0, y0, x1, y1 }
}

/// The whole pixel `value` sits on, failing when it sits between two.
///
/// Exactly whole, not nearly: a boundary a millionth of a pixel past the edge
/// tints the pixel beyond it, which is the blur the fitter exists to remove,
/// so a near-miss has to fail here.
fn whole(value: f64) -> i32 {
    assert!(
        value.total_cmp(&mathf::round(value)) == Ordering::Equal,
        "{value} is not a whole pixel"
    );
    mathf::round_i32(value)
}

/// Whether every coordinate of two outlines is bit-for-bit the same.
fn unmoved(before: &[Segment], after: &[Segment], axis: fn(&Segment) -> (f64, f64)) -> bool {
    before.iter().zip(after).all(|(before, after)| {
        let ((b0, b1), (a0, a1)) = (axis(before), axis(after));
        b0.total_cmp(&a0) == Ordering::Equal && b1.total_cmp(&a1) == Ordering::Equal
    })
}

/// A filled axis-aligned rectangle, wound clockwise in the rasteriser's
/// y-downward space so its interior fills.
fn rect(left: f64, top: f64, right: f64, bottom: f64) -> Vec<Segment> {
    alloc::vec![
        seg(left, top, right, top),
        seg(right, top, right, bottom),
        seg(right, bottom, left, bottom),
        seg(left, bottom, left, top),
    ]
}

/// The whole rows a fitted outline occupies, low then high.
fn rows(segments: &[Segment]) -> (i32, i32) {
    let (lo, hi) = segments.iter().fold((f64::MAX, f64::MIN), |(lo, hi), s| {
        (lo.min(s.y0).min(s.y1), hi.max(s.y0).max(s.y1))
    });
    (whole(lo), whole(hi))
}

/// The whole columns a fitted outline occupies, low then high.
fn columns(segments: &[Segment]) -> (i32, i32) {
    let (lo, hi) = segments.iter().fold((f64::MAX, f64::MIN), |(lo, hi), s| {
        (lo.min(s.x0).min(s.x1), hi.max(s.x0).max(s.x1))
    });
    (whole(lo), whole(hi))
}

/// Fit with no alignment zones, the way columns are always fitted and rows
/// are for a face that declares none.
fn fit_unzoned(segments: &mut [Segment], axes: Axes) {
    fit(segments, axes, &AlignZones::default(), 0.0, 1.0);
}

#[test]
fn a_stroke_straddling_two_pixels_lands_on_one() {
    // The defect the fitter exists for: 1.2 pixels of stem spread over two
    // columns at 60% each, instead of one solid column.
    let mut stem = rect(1.3, 3.4, 2.5, 12.6);
    fit_unzoned(&mut stem, Axes::RowsAndColumns);
    assert_eq!(columns(&stem), (1, 2), "1.2 columns of stem draw one");
    assert_eq!(rows(&stem), (4, 13), "and 9.2 rows of it draw nine");
}

#[test]
fn a_hairline_thickens_to_a_pixel_rather_than_fading_away() {
    // Rounding each edge on its own would collapse anything under half a
    // pixel to nothing, which is how a rule or a box border disappears.
    let mut hairline = rect(2.4, 4.0, 2.7, 9.0);
    fit_unzoned(&mut hairline, Axes::RowsAndColumns);
    let (left, right) = columns(&hairline);
    assert_eq!(right - left, 1, "0.3px of stem still draws a whole pixel");
}

#[test]
fn a_diagonal_is_left_alone() {
    // Snapping a diagonal's flanks would turn it into a staircase, so
    // anything steeper than the edge slope must come through untouched.
    let diagonal = alloc::vec![
        seg(1.3, 12.7, 6.4, 2.2),
        seg(6.4, 2.2, 7.1, 2.6),
        seg(7.1, 2.6, 2.0, 13.1),
        seg(2.0, 13.1, 1.3, 12.7),
    ];
    let mut fitted = diagonal.clone();
    fit_unzoned(&mut fitted, Axes::RowsAndColumns);
    assert!(
        unmoved(&diagonal, &fitted, |s| (s.x0, s.x1)),
        "the diagonal moved sideways"
    );
    assert!(
        unmoved(&diagonal, &fitted, |s| (s.y0, s.y1)),
        "the diagonal moved vertically"
    );
}

#[test]
fn rows_only_fitting_leaves_every_column_exactly_where_it_was() {
    // Proportional text is laid out by unfitted advances, so ink that moved
    // sideways would drift out from under the run that placed it.
    let before = rect(1.3, 3.4, 2.5, 12.6);
    let mut after = before.clone();
    fit_unzoned(&mut after, Axes::Rows);
    assert!(unmoved(&before, &after, |s| (s.x0, s.x1)), "a column moved");
    assert_eq!(rows(&after), (4, 13), "rows still snapped");
}

#[test]
fn an_alignment_zone_lands_two_glyphs_on_one_row() {
    // The reason zones exist: `x` and `o` reach almost but not quite the same
    // height, and rounding each on its own leaves a line of text ragged.
    let zones = AlignZones::new(alloc::vec![Zone::new(-7.0, -7.4)]);
    let fitted_top = |top: f64| {
        let mut glyph = rect(1.0, top, 3.0, 13.0);
        fit(&mut glyph, Axes::Rows, &zones, 13.0, 1.0);
        rows(&glyph).0
    };
    assert_eq!(fitted_top(6.0), 6, "the flat top keeps its row");
    assert_eq!(fitted_top(5.6), 6, "and the overshoot joins it");
}

#[test]
fn an_overshoot_worth_a_pixel_is_kept() {
    // Flattening the overshoot is only right while it cannot be drawn. At a
    // size where it is a pixel or more the design meant it, and a round
    // letter that lost it would sit visibly short of its neighbours.
    let zones = AlignZones::new(alloc::vec![Zone::new(-7.0, -7.4)]);
    let mut glyph = rect(1.0, 25.6, 3.0, 52.0);
    fit(&mut glyph, Axes::Rows, &zones, 52.0, 4.0);
    assert_eq!(rows(&glyph).0, 26, "rounded on its own, not to the zone");
}

#[test]
fn fitting_never_folds_an_outline_over_itself() {
    // The one invariant the whole warp rests on: it is monotone. Two
    // coordinates that were ordered stay ordered, so no contour turns inside
    // out however the strokes and zones pull at it.
    let zones = AlignZones::new(alloc::vec![Zone::new(0.0, 0.3), Zone::new(-7.0, -7.4)]);
    let mut seed = 0x243f_6a88_85a3_08d3u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        f64::from(u16::try_from(seed % 16_000).unwrap_or(0)) / 1000.0
    };
    for _ in 0..200 {
        let before: Vec<Segment> = (0..24)
            .map(|_| seg(next(), next(), next(), next()))
            .collect();
        let mut after = before.clone();
        fit(&mut after, Axes::RowsAndColumns, &zones, 13.0, 1.0);
        let coordinates = |list: &[Segment]| -> Vec<(f64, f64)> {
            list.iter()
                .flat_map(|s| [(s.x0, s.y0), (s.x1, s.y1)])
                .collect()
        };
        let (before, after) = (coordinates(&before), coordinates(&after));
        for (i, &(bx, by)) in before.iter().enumerate() {
            for (j, &(ox, oy)) in before.iter().enumerate() {
                if bx < ox {
                    assert!(after[i].0 <= after[j].0, "columns reordered");
                }
                if by < oy {
                    assert!(after[i].1 <= after[j].1, "rows reordered");
                }
            }
        }
    }
}

#[test]
fn an_outline_too_intricate_to_be_a_glyph_is_filled_unfitted() {
    // Pairing edges probes the outline once per edge, so an adversarial face
    // whose every segment is its own flat would cost the square of the
    // segment count. Past the cap the outline is left alone: still drawn,
    // just not fitted, and in time the rasteriser was always going to spend.
    let flats: Vec<Segment> = (0..1000)
        .map(|i| {
            let row = f64::from(i) * 0.7;
            seg(1.25, row, 6.25, row)
        })
        .collect();
    let mut fitted = flats.clone();
    fit_unzoned(&mut fitted, Axes::Rows);
    assert!(unmoved(&flats, &fitted, |s| (s.y0, s.y1)), "it was fitted");
}

#[test]
fn an_empty_outline_is_left_empty() {
    let mut none: Vec<Segment> = Vec::new();
    fit_unzoned(&mut none, Axes::RowsAndColumns);
    assert!(none.is_empty());
}

#[test]
fn two_edges_facing_the_same_way_are_not_a_stroke() {
    // A stem with a small bump on the same side, which is how the committed
    // face draws `j`: the dot juts a fraction of a pixel further right than
    // the stem below it, just far enough not to cluster into the stem's edge.
    // Both those sides face right, so they are not the two sides of anything
    // — but they are the closest pair in the glyph, and pairing on proximity
    // alone takes them first. The stem's own sides are then left unpaired and
    // its width is never held, so it thins away between two columns.
    const STEM_LEFT: usize = 7;
    const STEM_RIGHT: usize = 1;
    let mut glyph = alloc::vec![
        seg(4.7, 2.0, 5.7, 2.0),
        seg(5.7, 2.0, 5.7, 2.8),
        seg(5.7, 2.8, 6.1, 2.8),
        seg(6.1, 2.8, 6.1, 3.2),
        seg(6.1, 3.2, 5.7, 3.2),
        seg(5.7, 3.2, 5.7, 13.0),
        seg(5.7, 13.0, 4.7, 13.0),
        seg(4.7, 13.0, 4.7, 2.0),
    ];
    fit_unzoned(&mut glyph, Axes::RowsAndColumns);
    assert_eq!(
        whole(glyph[STEM_RIGHT].x0) - whole(glyph[STEM_LEFT].x0),
        1,
        "the stem lost its width"
    );
}

#[test]
fn a_stroke_drawn_in_two_runs_is_still_one_stroke() {
    // The `g` and `m` regression: a stroke's side need not be one run. An
    // `m`'s top is the left stem's flat and the two arch crowns, with the
    // valley over the middle stem between them; a `g`'s is its bowl's crown
    // and its ear. Asking whether the outline is filled halfway along the
    // *span* of such a side asks in the valley, reads no ink, and leaves the
    // stroke unrecognised — the arch then closes to a third of a pixel and
    // the ear vanishes with the top it hung from. Two bars at one row are
    // that shape stripped to its bones; the far one is written first so the
    // runs need ordering as well as keeping.
    let mut bar = rect(5.0, 3.4, 7.0, 4.2);
    bar.extend(rect(1.0, 3.4, 3.0, 4.2));
    fit_unzoned(&mut bar, Axes::Rows);
    assert_eq!(rows(&bar), (3, 4), "0.8 rows of bar draw one");
}

#[test]
fn a_long_shallow_run_is_not_an_edge() {
    // A segment can sit within the edge slope and still wander pixels across:
    // ten rows at one in seven drifts a pixel and a half. Snapping that onto
    // a column does not sharpen a stem, it shears a diagonal — which is what
    // the arms of a `w` are made of.
    let slanted = alloc::vec![
        seg(1.0, 2.0, 2.2, 2.0),
        seg(2.2, 2.0, 3.7, 12.0),
        seg(3.7, 12.0, 2.5, 12.0),
        seg(2.5, 12.0, 1.0, 2.0),
    ];
    let mut fitted = slanted.clone();
    fit_unzoned(&mut fitted, Axes::RowsAndColumns);
    assert!(
        unmoved(&slanted, &fitted, |s| (s.x0, s.x1)),
        "a slanted run was snapped onto a column"
    );
}

#[test]
fn evenly_spaced_stems_stay_evenly_spaced() {
    // The three stems of an `m`, at the spacing the committed face draws
    // them: even to a fortieth of a pixel. Rounding each stem's own centre
    // lets equal gaps round different ways and the letter comes out as
    // `|..|.|`, so every stroke is placed by the one shift the first sets.
    let mut stems = Vec::new();
    for left in [0.718, 3.537, 6.356] {
        stems.extend(rect(left, 6.0, left + 1.021, 13.0));
    }
    fit_unzoned(&mut stems, Axes::RowsAndColumns);
    let lefts: Vec<i32> = stems.chunks(4).map(|stem| whole(stem[0].x0)).collect();
    assert_eq!(
        lefts[1] - lefts[0],
        lefts[2] - lefts[1],
        "the stems came out at {lefts:?}"
    );
}

#[test]
fn fitting_moves_a_glyphs_ink_rather_than_destroying_it() {
    // Snapping redistributes ink; it must not lose a part of the letter.
    // Every way this has gone wrong shows up here: a stem erased by a bad
    // stroke pairing (`j`), the right of a bowl folded onto its neighbour
    // (`5`, `8`), an arm sheared into the one beside it (`w`).
    //
    // Some loss is the point rather than a fault — a 1.2-pixel stem drawn as
    // one solid pixel is a sixth less ink and far more legible, and the
    // thinnest mark the face draws, the apostrophe, is the extreme at four
    // fifths. Losing a stem costs several times that.
    const KEPT_PERCENT: u32 = 75;
    let (bytes, geometry) = console_cell();
    let face = Face::parse(&bytes).expect("parses");
    let area = |coverage: &[u8]| coverage.iter().map(|&level| u32::from(level)).sum::<u32>();
    for code in 0x21..0x7f {
        let glyph = face.glyph_for(code).expect("ASCII is covered");
        let fitted = face
            .rasterise_glyph(glyph, &geometry, f64::from(ATLAS_EM_PX), geometry.width)
            .expect("rasterises");
        let unfitted = face
            .rasterise_unfitted(glyph, &geometry, f64::from(ATLAS_EM_PX), geometry.width)
            .expect("rasterises");
        let kept = area(&fitted) * 100 / area(&unfitted).max(1);
        assert!(kept >= KEPT_PERCENT, "U+{code:04X} kept {kept}% of its ink");
    }
}

#[test]
fn a_stemmed_letter_draws_a_whole_stem() {
    // The `j` regression as the reader saw it: the stem was simply not there.
    // A letter built on a vertical stem must have a column that is solid from
    // the x-height to the baseline.
    let (bytes, geometry) = console_cell();
    let face = Face::parse(&bytes).expect("parses");
    for ch in [
        'b', 'd', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'p', 'q', 'r', 'u',
    ] {
        let glyph = face.glyph_for(u32::from(ch)).expect("covered");
        let coverage = face
            .rasterise_glyph(glyph, &geometry, f64::from(ATLAS_EM_PX), geometry.width)
            .expect("rasterises");
        let tallest = (0..geometry.width as usize)
            .map(|column| {
                coverage
                    .chunks(geometry.width as usize)
                    .fold((0u32, 0u32), |(run, best), row| {
                        let run = if row[column] == 15 { run + 1 } else { 0 };
                        (run, best.max(run))
                    })
                    .1
            })
            .max()
            .unwrap_or(0);
        assert!(tallest >= 6, "{ch} has no stem: {tallest} solid rows");
    }
}

#[test]
fn the_three_stems_of_an_m_are_evenly_spaced() {
    // The same evenness the synthetic case pins, on the letter that showed
    // it: the stems came out at columns 1, 4 and 6 rather than 1, 4 and 7.
    let (bytes, geometry) = console_cell();
    let face = Face::parse(&bytes).expect("parses");
    let glyph = face.glyph_for(u32::from('m')).expect("covered");
    let coverage = face
        .rasterise_glyph(glyph, &geometry, f64::from(ATLAS_EM_PX), geometry.width)
        .expect("rasterises");
    let stems: Vec<usize> = (0..geometry.width as usize)
        .filter(|&column| {
            coverage
                .chunks(geometry.width as usize)
                .filter(|row| row[column] == 15)
                .count()
                >= 6
        })
        .collect();
    assert_eq!(stems.len(), 3, "m has stems at {stems:?}");
    assert_eq!(
        stems[1] - stems[0],
        stems[2] - stems[1],
        "m has stems at {stems:?}"
    );
}

#[test]
fn an_arched_letter_draws_a_whole_arch() {
    // The `g` and `m` regression as the reader saw it: "the top of g and m
    // are missing". An arch or a bar is a horizontal stroke and is fitted to
    // a whole pixel exactly as a stem is; left at the four fifths of one the
    // face draws, it came out at a third of full coverage beneath solid
    // stems, which reads as no top at all. This is the stem test on its
    // side, over the letters whose design puts a stroke along their top.
    const SOLID: u8 = 10;
    const RUN: usize = 3;
    let (bytes, geometry) = console_cell();
    let face = Face::parse(&bytes).expect("parses");
    for ch in [
        'a', 'c', 'e', 'f', 'g', 'm', 'n', 'r', 's', 'z', 'B', 'D', 'E', 'F', 'P', 'R', 'T', 'Z',
        '2', '3', '5', '7',
    ] {
        let glyph = face.glyph_for(u32::from(ch)).expect("covered");
        let coverage = face
            .rasterise_glyph(glyph, &geometry, f64::from(ATLAS_EM_PX), geometry.width)
            .expect("rasterises");
        let top = coverage
            .chunks(geometry.width as usize)
            .find(|row| row.iter().any(|&level| level != 0))
            .expect("the letter draws");
        let widest = top
            .iter()
            .fold((0usize, 0usize), |(run, best), &level| {
                let run = if level >= SOLID { run + 1 } else { 0 };
                (run, best.max(run))
            })
            .1;
        assert!(widest >= RUN, "{ch} tops out at {widest} solid cells");
    }
}

/// The console atlas cell and the face it is rasterised from.
fn console_cell() -> (Vec<u8>, CellGeometry) {
    let bytes = asset("mono/Inconsolata-EX.ttf");
    let geometry = {
        let face = Face::parse(&bytes).expect("parses");
        let advance = face.uniform_advance().expect("monospace");
        CellGeometry::derive(&face, advance, ATLAS_EM_PX).expect("geometry")
    };
    (bytes, geometry)
}

#[test]
fn the_console_face_draws_mostly_solid_ink_at_its_own_cell() {
    // The regression guard for the whole change. Unfitted, this face put
    // under 9% of its ink at full coverage and 47% at mid-grey at the 8x16
    // console cell, which is what "the fonts are blurry" looks like as a
    // number; FreeType's autofitter reaches about 27% on the same face and
    // size. Anything back near the unfitted figure means the fitter has
    // stopped working.
    let (bytes, geometry) = console_cell();
    let face = Face::parse(&bytes).expect("parses");
    let mut levels = [0u32; 16];
    for code in 0x21..0x7f {
        let glyph = face.glyph_for(code).expect("ASCII is covered");
        let coverage = face
            .rasterise_glyph(glyph, &geometry, f64::from(ATLAS_EM_PX), geometry.width)
            .expect("rasterises");
        for &level in &coverage {
            levels[usize::from(level & 15)] += 1;
        }
    }
    let ink: u32 = levels[1..].iter().sum();
    let solid = levels[15] * 100 / ink;
    let grey = levels[4..12].iter().sum::<u32>() * 100 / ink;
    assert!(solid >= 33, "only {solid}% of the ink is solid");
    assert!(grey <= 32, "{grey}% of the ink is mid-grey");
}

#[test]
fn no_console_glyph_reaches_into_the_next_cell() {
    // The face advances 613/1024 em, which is 8.38 pixels in the 8-pixel cell
    // that advance defines, so drawn at its own width every glyph overhangs
    // and the rightmost stems touch the next character.
    let (bytes, geometry) = console_cell();
    let face = Face::parse(&bytes).expect("parses");
    let width = geometry.width * 2;
    for code in 0x21..0x7f {
        let glyph = face.glyph_for(code).expect("ASCII is covered");
        let coverage = face
            .rasterise_glyph(glyph, &geometry, f64::from(ATLAS_EM_PX), width)
            .expect("rasterises");
        let spill = coverage
            .chunks(width as usize)
            .flat_map(|row| &row[geometry.width as usize..])
            .filter(|&&level| level > 0)
            .count();
        assert_eq!(spill, 0, "U+{code:04X} puts ink in the next cell");
    }
}

#[test]
fn console_letters_share_a_baseline_and_an_x_height() {
    // Zones in practice: a flat letter, a round one and a diagonal one all
    // start and stop on the same rows, so a line of text does not ripple.
    let (bytes, geometry) = console_cell();
    let face = Face::parse(&bytes).expect("parses");
    let extent = |ch: char| {
        let glyph = face.glyph_for(u32::from(ch)).expect("covered");
        let coverage = face
            .rasterise_glyph(glyph, &geometry, f64::from(ATLAS_EM_PX), geometry.width)
            .expect("rasterises");
        let inked: Vec<u32> = coverage
            .chunks(geometry.width as usize)
            .enumerate()
            .filter(|(_, row)| row.iter().any(|&level| level > 0))
            .map(|(row, _)| u32::try_from(row).unwrap_or(0))
            .collect();
        (
            inked.first().copied().unwrap_or(0),
            inked.last().copied().unwrap_or(0),
        )
    };
    let (top, bottom) = extent('x');
    for ch in ['o', 'c', 'e', 's', 'z', 'v', 'w', 'n', 'm', 'u', 'r'] {
        assert_eq!(extent(ch), (top, bottom), "{ch} sits off the line");
    }
    let (cap_top, cap_bottom) = extent('H');
    assert_eq!(cap_bottom, bottom, "capitals share the baseline");
    for ch in ['E', 'T', 'I', 'L', 'F'] {
        assert_eq!(extent(ch), (cap_top, cap_bottom), "{ch} sits off the line");
    }
}
