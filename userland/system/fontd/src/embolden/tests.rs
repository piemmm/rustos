//! Unit tests for the synthetic-emboldening coverage transform.

use super::{embolden, stroke_subpixels, SUBPIXEL};
use tairix_abi::font_ipc::{FontWeight, FONT_MAX_WEIGHT, FONT_MIN_WEIGHT};

/// The rendered em of a `px`-tall glyph, in the 1/256 px unit the stroke
/// arithmetic is carried in.
const fn em_subpixels(px: u32) -> u32 {
    px * SUBPIXEL
}

#[test]
fn a_regular_stroke_of_zero_leaves_the_coverage_byte_identical() {
    let original = [0u8, 255, 128, 0, 64, 0];
    let mut coverage = original;

    embolden(&mut coverage, 3, 0);

    assert_eq!(coverage, original);
}

#[test]
fn a_whole_pixel_stroke_smears_ink_one_pixel_right_per_row() {
    let mut coverage = [0u8, 200, 0, 0, 0, 90];

    embolden(&mut coverage, 3, SUBPIXEL);

    assert_eq!(coverage, [0, 200, 200, 0, 0, 90]);
}

#[test]
fn a_stroke_never_wraps_ink_into_the_next_row() {
    let mut coverage = [0u8, 0, 255, 0, 0, 0];

    embolden(&mut coverage, 3, 2 * SUBPIXEL);

    assert_eq!(coverage[3..], [0, 0, 0]);
}

#[test]
fn a_fractional_stroke_darkens_the_edge_proportionally() {
    let mut coverage = [255u8, 0];

    embolden(&mut coverage, 2, SUBPIXEL / 2);

    assert_eq!(coverage, [255, 127]);
}

#[test]
fn a_stroke_wider_than_one_pixel_carries_its_fractional_tail() {
    let mut coverage = [255u8, 0, 0, 0];

    embolden(&mut coverage, 4, SUBPIXEL + SUBPIXEL / 4);

    assert_eq!(coverage, [255, 255, 63, 0]);
}

#[test]
fn coverage_never_exceeds_full_opacity() {
    let mut coverage = [255u8; 8];

    embolden(&mut coverage, 8, 3 * SUBPIXEL);

    assert_eq!(coverage, [255u8; 8]);
}

#[test]
fn a_degenerate_bitmap_is_left_alone_rather_than_indexed_out_of_range() {
    let mut empty: [u8; 0] = [];
    embolden(&mut empty, 4, SUBPIXEL);

    let mut short = [7u8, 9];
    embolden(&mut short, 4, SUBPIXEL);
    assert_eq!(short, [7, 9]);

    let mut zero_width = [7u8, 9];
    embolden(&mut zero_width, 0, SUBPIXEL);
    assert_eq!(zero_width, [7, 9]);
}

#[test]
fn the_stroke_scales_with_the_rendered_em_size() {
    let small = stroke_subpixels(em_subpixels(16), FontWeight::BOLD);
    let double = stroke_subpixels(em_subpixels(32), FontWeight::BOLD);

    assert_eq!(small, (em_subpixels(16) + 12) / 24);
    assert!(double.abs_diff(2 * small) <= 1);
}

#[test]
fn the_stroke_ramps_linearly_with_the_axis_above_regular() {
    let em = em_subpixels(18);
    let weight = |axis| FontWeight::new(axis).expect("a weight on the axis");

    assert_eq!(stroke_subpixels(em, FontWeight::REGULAR), 0);

    let half = stroke_subpixels(em, weight(550));
    let bold = stroke_subpixels(em, FontWeight::BOLD);
    assert!(half > 0 && half < bold);
    assert!(bold.abs_diff(2 * half) <= 1, "{bold} is not twice {half}");

    // Every step of the axis is resolved, rather than snapping to the
    // nearest named weight.
    assert!(stroke_subpixels(em, weight(500)) < stroke_subpixels(em, weight(600)));
}

#[test]
fn a_weight_at_or_below_regular_adds_no_stroke() {
    let em = em_subpixels(18);
    for axis in [FONT_MIN_WEIGHT, 100, 399, 400] {
        let weight = FontWeight::new(axis).expect("a weight on the axis");
        assert_eq!(stroke_subpixels(em, weight), 0, "axis {axis}");
    }
}

#[test]
fn a_degenerate_em_size_yields_no_stroke_rather_than_wrapping() {
    assert_eq!(stroke_subpixels(0, FontWeight::BOLD), 0);
    assert_eq!(stroke_subpixels(u32::MAX, FontWeight::REGULAR), 0);

    // Still about a twenty-fourth of the em, and nowhere near a wrap.
    let widest = stroke_subpixels(u32::MAX, FontWeight::BOLD);
    assert!(
        widest > u32::MAX / 25 && widest < u32::MAX / 23,
        "{widest} is not about a twenty-fourth of the em"
    );
}

#[test]
fn a_weight_past_bold_goes_on_thickening() {
    let em = em_subpixels(18);
    let heaviest = FontWeight::new(FONT_MAX_WEIGHT).expect("the axis maximum");
    assert!(stroke_subpixels(em, heaviest) > stroke_subpixels(em, FontWeight::BOLD));
}
