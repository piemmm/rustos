//! Unit tests for the shared box blur and the frost built on it.
//!
//! They pin down the identities every frosted surface relies on: a uniform
//! field survives exactly, an impulse spreads symmetrically and conserves
//! its energy, alpha blurs with the colour channels and keeps the
//! premultiplied invariant, and every degenerate shape (one pixel, one row,
//! one column, a radius wider than the region) is answered rather than
//! refused. The rest are the fail-closed refusals, and [`frost_region`]'s
//! own contract: what it touches, what the coverage weight does, which
//! coordinates the shape is read at, and that a reused scratch carries
//! nothing between calls.
//!
//! [`frost_region`]: Surface::frost_region

use core::cell::RefCell;
use core::ops::Range;

use alloc::vec;
use alloc::vec::Vec;

use tairix_rng::RandU64;

use super::{box_blur, BlurScratch, Reciprocal, RECIPROCAL_MAX_COUNT, RECIPROCAL_SHIFT};
use crate::color::{div255_biased, Pixel, ROUND_NEAREST};
use crate::dither::DitherRow;
use crate::round::round_rect_coverage;
use crate::surface::Surface;

/// An opaque grey.
fn grey(v: u8) -> Pixel {
    Pixel {
        r: v,
        g: v,
        b: v,
        a: 255,
    }
}

/// Blur `region` (given as `width`×`height`) and hand back the result.
fn blurred(region: &[Pixel], width: usize, height: usize, radius: usize) -> Vec<Pixel> {
    let mut pixels = region.to_vec();
    let mut aux = vec![Pixel::TRANSPARENT; width * height];
    box_blur(&mut pixels, width, height, radius, &mut aux);
    pixels
}

/// A `width`×`height` surface whose every pixel differs, so a blur that
/// reads or writes the wrong one cannot hide behind a flat field.
fn patterned(width: u32, height: u32) -> Surface {
    let mut surface = Surface::new(width, height).expect("allocates");
    for y in 0..height {
        for x in 0..width {
            let channel = |factor: u32| u8::try_from((x * factor + y * 13) % 256).unwrap_or(0);
            surface.set(
                x,
                y,
                Pixel {
                    r: channel(7).min(channel(53)),
                    g: channel(11).min(channel(53)),
                    b: channel(29).min(channel(53)),
                    a: channel(53),
                },
            );
        }
    }
    surface
}

/// A box blur written the obvious way: two separable passes, each averaging
/// the window around every sample by clamping the sample index to the line and
/// dividing.
///
/// The oracle the sliding-window implementation is proved against. It shares
/// nothing with it — no running sum, no reciprocal, no iterator arithmetic —
/// so an error in either shows up as a differing pixel. It is `O(area·radius)`
/// and only ever run over the small regions these tests use.
fn naive_box_blur(region: &[Pixel], width: usize, height: usize, radius: usize) -> Vec<Pixel> {
    let reach = isize::try_from(radius).expect("a test radius fits");
    let count = u32::try_from(radius * 2 + 1).expect("a test radius fits");
    let mean = |sum: u32| -> u8 {
        u8::try_from(((sum + count / 2) / count).min(255)).expect("clamped to the channel range")
    };
    let average = |field: &[Pixel], x: usize, y: usize, horizontal: bool| -> Pixel {
        let (along, limit) = if horizontal {
            (isize::try_from(x).expect("in range"), width - 1)
        } else {
            (isize::try_from(y).expect("in range"), height - 1)
        };
        let (mut r, mut g, mut b, mut a) = (0u32, 0u32, 0u32, 0u32);
        for step in -reach..=reach {
            let at = usize::try_from(along + step).unwrap_or(0).min(limit);
            let pixel = if horizontal {
                field[y * width + at]
            } else {
                field[at * width + x]
            };
            r += u32::from(pixel.r);
            g += u32::from(pixel.g);
            b += u32::from(pixel.b);
            a += u32::from(pixel.a);
        }
        Pixel {
            r: mean(r),
            g: mean(g),
            b: mean(b),
            a: mean(a),
        }
    };

    if radius == 0 || width == 0 || height == 0 {
        return region.to_vec();
    }
    let mut mid = region.to_vec();
    for y in 0..height {
        for x in 0..width {
            mid[y * width + x] = average(region, x, y, true);
        }
    }
    let mut out = mid.clone();
    for y in 0..height {
        for x in 0..width {
            out[y * width + x] = average(&mid, x, y, false);
        }
    }
    out
}

#[test]
fn the_blur_is_the_naive_average_for_every_shape_and_radius() {
    // The one identity the sliding window, the hoisted bounds checks, and the
    // reciprocal multiply must all preserve: the same bytes the obvious
    // implementation produces. Includes the shapes that exercise the clamped
    // ends — a single row, a single column, and a radius wider than the region,
    // where every sample of some window is a replicated edge.
    let mut rng = TestRng::new(0x51ED_0B10_0BED_51ED);
    for (width, height) in [
        (1usize, 1usize),
        (1, 9),
        (9, 1),
        (2, 2),
        (5, 3),
        (7, 7),
        (16, 9),
        (13, 11),
    ] {
        let field: Vec<Pixel> = (0..width * height).map(|_| rng.pixel()).collect();
        for radius in [0usize, 1, 2, 3, 4, 8, 17] {
            assert_eq!(
                blurred(&field, width, height, radius),
                naive_box_blur(&field, width, height, radius),
                "a {width}x{height} region at radius {radius}"
            );
        }
    }
}

#[test]
fn the_reciprocal_is_exact_for_every_window_the_blur_uses() {
    // The proof, checked rather than argued: for every window size the blur
    // resolves a reciprocal for, the excess `e = m*d - 2^S` is small enough
    // that `floor(n*m / 2^S)` and `floor(n/d)` cannot differ for any numerator
    // a window can produce, and the product cannot overflow the `u64` it is
    // formed in.
    let scale = 1u64 << RECIPROCAL_SHIFT;
    for d in 1..=u64::from(RECIPROCAL_MAX_COUNT) {
        let m = scale.div_ceil(d);
        let excess = m * d - scale;
        assert!(excess < d, "d={d}: the excess must stay below the divisor");
        // A window holds `d` samples of at most 255, plus the rounding half.
        let n_max = 256 * d - 1;
        assert!(
            n_max * excess < scale,
            "d={d}: n_max*e = {} reaches 2^{RECIPROCAL_SHIFT}",
            n_max * excess
        );
        assert!(
            n_max
                .checked_mul(m)
                .is_some_and(|product| product < 1 << 49),
            "d={d}: the reciprocal product must stay well inside a u64"
        );
    }

    // And the cutoff is where the proof stops holding, not a comfortable guess:
    // above it there are counts whose excess is too large, which is why the
    // divide has to stay.
    let beyond = u64::from(RECIPROCAL_MAX_COUNT) + 1;
    let breaks = (beyond..=beyond * 2).any(|d| {
        let excess = scale.div_ceil(d) * d - scale;
        (256 * d - 1) * excess >= scale
    });
    assert!(breaks, "the cutoff could be raised, so it is arbitrary");
}

#[test]
fn the_reciprocal_answers_exactly_what_the_divide_would() {
    // The reference is written out here rather than called, so this is an
    // oracle and not a restatement of the code under test.
    let reference = |sum: u32, count: u32| -> u8 {
        let divisor = u64::from(count.max(1));
        let rounded = (u64::from(sum) + divisor / 2) / divisor;
        u8::try_from(rounded.min(255)).expect("clamped to the channel range")
    };
    let checked = |count: u32, sum: u32| {
        let recip = Reciprocal::new(usize::try_from(count).expect("a test count fits"));
        assert_eq!(
            recip.apply(sum),
            reference(sum, count),
            "count {count}, sum {sum}"
        );
    };

    // Every reachable sum, exhaustively, for the radii a desktop actually
    // frosts at (1 to 4) plus the degenerate single-sample window.
    for radius in 0..=4u32 {
        let count = radius * 2 + 1;
        for sum in 0..=255 * count {
            checked(count, sum);
        }
    }

    // Boundaries and a spread for the large counts, including the first one
    // past the reciprocal's range and one whose numerator saturates.
    let mut rng = TestRng::new(0xD15C_0FFE_ED15_C0DE);
    for count in [
        255u32,
        257,
        4095,
        RECIPROCAL_MAX_COUNT - 1,
        RECIPROCAL_MAX_COUNT,
        RECIPROCAL_MAX_COUNT + 1,
        u32::MAX,
    ] {
        let full = 255u32.saturating_mul(count);
        for sum in [
            0,
            1,
            count / 2 - 1,
            count / 2,
            count / 2 + 1,
            count,
            full / 255 * 254,
            full,
            u32::MAX,
        ] {
            checked(count, sum);
        }
        for _ in 0..64 {
            checked(count, rng.next_u32() % full.saturating_add(1));
        }
    }
}

/// A deterministic generator for the differential sweeps, so a failure is
/// reproducible from the seed alone.
///
/// It is `lib/rng`'s own non-cryptographic generator rather than a private one
/// written here, so the workspace keeps a single fast generator.
struct TestRng(tairix_rng::FastRng);

impl TestRng {
    fn new(seed: u64) -> Self {
        Self(tairix_rng::FastRng::seed_from_u64(seed))
    }

    fn next_u32(&mut self) -> u32 {
        u32::try_from(self.0.next_u64() >> 32).unwrap_or(u32::MAX)
    }

    /// A pixel with independent channels, alpha included, so a blur that
    /// crosses channels is caught.
    fn pixel(&mut self) -> Pixel {
        let bytes = self.0.next_u64().to_le_bytes();
        Pixel {
            r: bytes[0],
            g: bytes[1],
            b: bytes[2],
            a: bytes[3],
        }
    }
}

#[test]
fn uniform_field_is_unchanged() {
    let field = vec![grey(40); 7 * 5];
    assert_eq!(blurred(&field, 7, 5, 2), field);
}

#[test]
fn radius_zero_is_identity() {
    let mut field = vec![grey(10); 9];
    field[4] = grey(200);
    assert_eq!(blurred(&field, 3, 3, 0), field);
}

#[test]
fn impulse_spreads_symmetrically() {
    let mut field = vec![grey(0); 5 * 5];
    field[2 * 5 + 2] = grey(255);
    let out = blurred(&field, 5, 5, 1);

    let centre = out[2 * 5 + 2];
    assert!(centre.r > 0 && centre.r < 255, "the impulse spread out");
    for (x, y) in [(1, 2), (3, 2), (2, 1), (2, 3)] {
        assert_eq!(
            out[y * 5 + x],
            centre,
            "a 3x3 box spreads an impulse equally over its whole window"
        );
    }
    assert_eq!(out[0], grey(0), "a pixel two columns away is untouched");
}

#[test]
fn impulse_conserves_total_energy() {
    let mut field = vec![Pixel::TRANSPARENT; 9 * 9];
    field[4 * 9 + 4] = grey(90);
    let out = blurred(&field, 9, 9, 1);
    let total: u32 = out.iter().map(|p| u32::from(p.r)).sum();
    assert_eq!(total, 90, "a box blur redistributes, it does not amplify");
}

#[test]
fn alpha_is_averaged_with_the_colour_channels() {
    let opaque = grey(200);
    let field = vec![
        opaque,
        opaque,
        opaque,
        Pixel::TRANSPARENT,
        Pixel::TRANSPARENT,
        Pixel::TRANSPARENT,
    ];
    let out = blurred(&field, 6, 1, 1);
    let edge = out[3];
    assert!(
        edge.a > 0 && edge.a < 255,
        "alpha blurs across the edge like every other channel"
    );
    assert!(
        edge.r <= edge.a,
        "the premultiplied invariant survives the blur"
    );
}

#[test]
fn single_pixel_region_is_returned_unchanged() {
    let field = vec![grey(77)];
    assert_eq!(blurred(&field, 1, 1, 4), field);
}

#[test]
fn single_column_and_single_row_regions_are_handled() {
    let column = vec![grey(0), grey(255), grey(0)];
    let out = blurred(&column, 1, 3, 3);
    assert_eq!(out[0], out[2], "a clamped column blurs symmetrically");
    assert!(out[1].r > 0 && out[1].r < 255);

    let row = vec![grey(0), grey(255), grey(0)];
    assert_eq!(blurred(&row, 3, 1, 3), out, "a row blurs like a column");
}

#[test]
fn radius_wider_than_the_region_still_averages() {
    let field = vec![grey(0), grey(120), grey(240), grey(0)];
    let out = blurred(&field, 4, 1, 64);
    assert!(
        out.windows(2).all(|pair| pair[0] == pair[1]),
        "a radius that swallows the region flattens it"
    );
}

#[test]
fn short_buffers_leave_the_region_untouched() {
    let field = vec![grey(10), grey(250), grey(10), grey(250)];
    let mut pixels = field.clone();
    let mut aux = vec![Pixel::TRANSPARENT; 2];
    box_blur(&mut pixels, 2, 2, 1, &mut aux[..1]);
    assert_eq!(pixels, field, "a short scratch buffer blurs nothing");

    let mut short = field[..3].to_vec();
    box_blur(&mut short, 2, 2, 1, &mut aux);
    assert_eq!(short, field[..3], "a short region blurs nothing");
}

/// The `w`×`h` block of `surface` at `(x, y)`, row-major.
fn block(surface: &Surface, x: u32, y: u32, w: u32, h: u32) -> Vec<Pixel> {
    let mut out = Vec::new();
    for row in y..y + h {
        for column in x..x + w {
            out.push(surface.get(column, row).expect("inside the surface"));
        }
    }
    out
}

/// `surface` with `[x, x+w) × [y, y+h)` frosted at a constant coverage.
fn frosted(
    mut surface: Surface,
    (x, y, w, h): (u32, u32, u32, u32),
    radius: u32,
    weight: u8,
) -> Surface {
    surface.frost_region(x, y, w, h, radius, &mut BlurScratch::new(), |_, _| weight);
    surface
}

/// Every pixel of `after` outside `[x, x+w) × [y, y+h)` still carries what
/// `before` had there.
fn outside_is_untouched(after: &Surface, before: &Surface, (x, y, w, h): (u32, u32, u32, u32)) {
    for row in 0..before.height() {
        for column in 0..before.width() {
            if (x..x + w).contains(&column) && (y..y + h).contains(&row) {
                continue;
            }
            assert_eq!(
                after.get(column, row),
                before.get(column, row),
                "({column}, {row}) is outside the frosted rectangle"
            );
        }
    }
}

#[test]
fn a_frost_blurs_its_region_and_nothing_else() {
    let rect = (3, 2, 6, 5);
    let before = patterned(12, 9);
    let after = frosted(before.clone(), rect, 2, 255);
    assert_ne!(
        block(&after, 3, 2, 6, 5),
        block(&before, 3, 2, 6, 5),
        "the rectangle is frosted"
    );
    outside_is_untouched(&after, &before, rect);
}

/// Full coverage takes the blurred pixel outright, so the rectangle comes
/// out bit-for-bit [`box_blur`] over a copy of it. Frosting a whole surface
/// at full coverage is the surface-wide blur this crate used to offer
/// separately, so its equivalence test lives here now.
#[test]
fn full_coverage_is_exactly_the_box_blur_of_the_region() {
    for radius in [1u32, 2, 5] {
        let before = patterned(9, 7);
        let expected = blurred(&block(&before, 2, 1, 6, 5), 6, 5, radius as usize);
        let after = frosted(before, (2, 1, 6, 5), radius, 255);
        assert_eq!(block(&after, 2, 1, 6, 5), expected, "at radius {radius}");
    }

    let before = patterned(9, 7);
    let expected = blurred(before.pixels(), 9, 7, 3);
    let after = frosted(before, (0, 0, 9, 7), 3, 255);
    assert_eq!(
        after.pixels(),
        expected,
        "frosting the whole surface is the blur of it"
    );
}

#[test]
fn zero_coverage_leaves_the_whole_surface_untouched() {
    let before = patterned(10, 8);
    let after = frosted(before.clone(), (1, 1, 7, 6), 3, 0);
    assert_eq!(after, before, "a coverage of nothing frosts nothing");
}

/// A partial coverage is a translucent field over a picture, so the mix takes
/// the surface's ordered dither: the expectation is the same weighted average
/// rounded at each pixel's own bias, read at its **surface** position (the
/// rectangle starts at `(1, 1)`), not at a fixed rounding.
#[test]
fn partial_coverage_is_the_dithered_weighted_mix_of_the_original_and_the_blur() {
    const WEIGHT: u8 = 64;
    const RECT: (u32, u32, u32, u32) = (1, 1, 5, 4);
    let before = patterned(8, 6);
    let original = block(&before, RECT.0, RECT.1, RECT.2, RECT.3);
    let blur = blurred(&original, 5, 4, 2);
    let keep = u32::from(255 - WEIGHT);
    let take = u32::from(WEIGHT);
    let mixed = |from: Pixel, to: Pixel, bias: u32| {
        let channel =
            |from: u8, to: u8| div255_biased(u32::from(from) * keep + u32::from(to) * take, bias);
        Pixel {
            r: channel(from.r, to.r),
            g: channel(from.g, to.g),
            b: channel(from.b, to.b),
            a: channel(from.a, to.a),
        }
    };
    let expected: Vec<Pixel> = original
        .iter()
        .zip(&blur)
        .enumerate()
        .map(|(index, (from, to))| {
            let column = RECT.0 + u32::try_from(index).unwrap_or(0) % RECT.2;
            let row = RECT.1 + u32::try_from(index).unwrap_or(0) / RECT.2;
            mixed(*from, *to, DitherRow::at(row).bias(column))
        })
        .collect();
    assert_ne!(expected, original, "a partial mix is not the original");
    assert_ne!(expected, blur, "a partial mix is not the blur");

    let after = frosted(before, RECT, 2, WEIGHT);
    assert_eq!(block(&after, RECT.0, RECT.1, RECT.2, RECT.3), expected);

    // The dither only ever chooses between the two levels the exact mix falls
    // between, so no pixel is more than a level from the undithered answer.
    for ((from, to), got) in original.iter().zip(&blur).zip(&expected) {
        let nearest = mixed(*from, *to, ROUND_NEAREST);
        for (a, b) in [
            (got.r, nearest.r),
            (got.g, nearest.g),
            (got.b, nearest.b),
            (got.a, nearest.a),
        ] {
            assert!(a.abs_diff(b) <= 1, "{a} is more than a level from {b}");
        }
    }
}

#[test]
fn a_rounded_coverage_frosts_the_centre_and_spares_the_corner() {
    const SIDE: u32 = 16;
    const RADIUS: u32 = 6;
    let before = patterned(SIDE, SIDE);
    let expected = blurred(before.pixels(), SIDE as usize, SIDE as usize, 2);
    let mut after = before.clone();
    after.frost_region(0, 0, SIDE, SIDE, 2, &mut BlurScratch::new(), |lx, ly| {
        round_rect_coverage(lx, ly, SIDE, SIDE, RADIUS)
    });

    assert_eq!(
        round_rect_coverage(0, 0, SIDE, SIDE, RADIUS),
        0,
        "the extreme corner lies outside the arc"
    );
    assert_eq!(
        after.get(0, 0),
        before.get(0, 0),
        "the extreme corner keeps the raw backdrop"
    );

    let centre = SIDE / 2;
    assert_eq!(round_rect_coverage(centre, centre, SIDE, SIDE, RADIUS), 255);
    assert_eq!(
        after.get(centre, centre),
        Some(expected[(centre * SIDE + centre) as usize]),
        "the centre is fully frosted"
    );
}

#[test]
fn a_region_clipped_by_the_surface_edge_frosts_only_what_is_on_it() {
    let before = patterned(8, 8);
    let mut after = before.clone();
    let asked = RefCell::new(Vec::new());
    after.frost_region(5, 6, 9, 7, 2, &mut BlurScratch::new(), |lx, ly| {
        asked.borrow_mut().push((lx, ly));
        255
    });

    let admitted: Vec<(u32, u32)> = (0..2)
        .flat_map(|ly| (0..3).map(move |lx| (lx, ly)))
        .collect();
    assert_eq!(
        asked.into_inner(),
        admitted,
        "only the part on the surface is asked about, at its rectangle-relative place"
    );
    assert_eq!(
        block(&after, 5, 6, 3, 2),
        blurred(&block(&before, 5, 6, 3, 2), 3, 2, 2),
        "the part on the surface is frosted"
    );
    outside_is_untouched(&after, &before, (5, 6, 3, 2));
    assert_eq!(
        after.pixels().len(),
        before.pixels().len(),
        "the buffer is the size it was"
    );
}

#[test]
fn a_clip_confines_the_frost_and_the_shape_still_reads_from_the_rectangle() {
    let before = patterned(10, 10);
    let mut after = before.clone();
    let asked = RefCell::new(Vec::new());
    after.with_clip(3, 4, 4, 3, |surface| {
        surface.frost_region(1, 2, 8, 7, 2, &mut BlurScratch::new(), |lx, ly| {
            asked.borrow_mut().push((lx, ly));
            255
        });
    });

    // The clip cuts two columns and two rows off the rectangle's leading
    // edges, so the shape is asked about from (2, 2) on, not from (0, 0).
    let admitted: Vec<(u32, u32)> = (2..5)
        .flat_map(|ly| (2..6).map(move |lx| (lx, ly)))
        .collect();
    assert_eq!(asked.into_inner(), admitted);
    assert_eq!(
        block(&after, 3, 4, 4, 3),
        blurred(&block(&before, 3, 4, 4, 3), 4, 3, 2),
        "what the clip admits is frosted"
    );
    outside_is_untouched(&after, &before, (3, 4, 4, 3));
}

/// `surface` with `[x, x+w) × [y, y+h)` frosted around the kept `cols` × `rows`
/// block, at a constant coverage.
fn frosted_around(
    mut surface: Surface,
    (x, y, w, h): (u32, u32, u32, u32),
    (cols, rows): (Range<u32>, Range<u32>),
    radius: u32,
    weight: u8,
) -> Surface {
    surface.frost_region_around(
        x,
        y,
        w,
        h,
        cols,
        rows,
        radius,
        &mut BlurScratch::new(),
        |_, _| weight,
    );
    surface
}

/// Frosting the border around a kept block, then putting back what the block
/// held before, is bit-for-bit the whole frost.
///
/// This is the compositor's drag path: the retained backdrop of a window that
/// has moved is still exact in the middle, so only the border is recomputed and
/// the middle is copied from the frost of the previous frame. If the border
/// disagreed with the whole frost by one level anywhere — a neighbourhood
/// clipped to the border, a replication edge read from the band instead of the
/// rectangle, a dither read from the band's own corner, or one band reading
/// another band's already-frosted pixels — the join would show as an edge that
/// travels with the window.
#[test]
fn a_frosted_border_around_a_kept_block_is_exactly_the_whole_frost() {
    let mut rng = TestRng::new(0x9A11_0F20_57D1_7E20);
    let rect = (2u32, 1, 13, 11);
    for radius in [1u32, 2, 3, 7] {
        // Full coverage, a rounded weight, and none at all: the mix and its
        // dither are part of what must agree, not just the blur.
        for weight in [255u8, 176, 0] {
            let before = patterned(17, 14);
            let whole = frosted(before.clone(), rect, radius, weight);
            for _ in 0..24 {
                let keep = (span_within(&mut rng, rect.2), span_within(&mut rng, rect.3));
                let mut border = frosted_around(before.clone(), rect, keep.clone(), radius, weight);
                let kept = (
                    rect.0 + keep.0.start,
                    rect.1 + keep.1.start,
                    keep.0.end - keep.0.start,
                    keep.1.end - keep.1.start,
                );
                // What the block held is the caller's own business: it kept the
                // earlier frost of those very pixels.
                assert_eq!(
                    block(&border, kept.0, kept.1, kept.2, kept.3),
                    block(&before, kept.0, kept.1, kept.2, kept.3),
                    "the kept block is left alone"
                );
                for row in kept.1..kept.1 + kept.3 {
                    for column in kept.0..kept.0 + kept.2 {
                        border.set(column, row, whole.get(column, row).expect("in the surface"));
                    }
                }
                assert_eq!(
                    border, whole,
                    "a radius-{radius} frost at coverage {weight} around the kept \
                     columns {:?} rows {:?}",
                    keep.0, keep.1
                );
            }
        }
    }
}

/// A non-empty sub-range of `0..extent`.
fn span_within(rng: &mut TestRng, extent: u32) -> Range<u32> {
    let start = rng.next_u32() % extent;
    let len = 1 + rng.next_u32() % (extent - start);
    start..start + len
}

/// A kept block touching an edge leaves fewer than four border bands, and a
/// block covering the whole rectangle leaves none — neither is a special case
/// the caller has to know about.
#[test]
fn a_kept_block_against_an_edge_still_frosts_exactly_the_rest() {
    let rect = (1u32, 2, 10, 8);
    let before = patterned(13, 12);
    let whole = frosted(before.clone(), rect, 2, 255);
    for keep in [
        (0..4, 0..8),  // the whole left side: no top, bottom, or left band
        (0..10, 0..3), // the whole top: only a bottom band
        (6..10, 5..8), // a corner: no bottom or right band
        (0..10, 0..8), // everything: nothing to frost at all
    ] {
        let mut border = frosted_around(before.clone(), rect, keep.clone(), 2, 255);
        for row in keep.1.clone() {
            for column in keep.0.clone() {
                let (x, y) = (rect.0 + column, rect.1 + row);
                border.set(x, y, whole.get(x, y).expect("in the surface"));
            }
        }
        assert_eq!(
            border, whole,
            "keeping columns {:?} rows {:?}",
            keep.0, keep.1
        );
    }
}

/// Keeping nothing is the whole frost, so a caller with an empty core need not
/// choose between two calls.
#[test]
fn keeping_nothing_frosts_the_whole_rectangle() {
    let rect = (2u32, 2, 9, 7);
    let before = patterned(13, 11);
    let whole = frosted(before.clone(), rect, 3, 255);
    for keep in [(0..0, 0..0), (4..4, 0..7), (0..9, 3..3), (99..120, 0..7)] {
        assert_eq!(
            frosted_around(before.clone(), rect, keep.clone(), 3, 255),
            whole,
            "keeping columns {:?} rows {:?} covers no pixels",
            keep.0,
            keep.1
        );
    }
}

#[test]
fn a_frost_with_nothing_to_do_leaves_the_surface_exactly_as_it_was() {
    let untouched = patterned(6, 4);

    let mut disabled = untouched.clone();
    disabled.frost_region(0, 0, 6, 4, 0, &mut BlurScratch::new(), |_, _| 255);
    assert_eq!(disabled, untouched, "a radius of zero frosts nothing");

    for (w, h) in [(0, 3), (3, 0), (0, 0)] {
        assert_eq!(
            frosted(untouched.clone(), (1, 1, w, h), 3, 255),
            untouched,
            "a {w}x{h} rectangle covers no pixels"
        );
    }

    // The last pair overflows `x + w`, which saturates to nothing rather
    // than wrapping back onto the surface.
    for (x, y) in [(6, 0), (0, 4), (99, 99), (u32::MAX, 0)] {
        assert_eq!(
            frosted(untouched.clone(), (x, y, 5, 5), 3, 255),
            untouched,
            "a rectangle at ({x}, {y}) lands off the surface"
        );
    }
}

#[test]
fn an_empty_surface_has_nothing_to_frost() {
    for (w, h) in [(0, 0), (0, 5), (5, 0)] {
        let mut surface = Surface::new(w, h).expect("allocates");
        surface.frost_region(0, 0, 5, 5, 3, &mut BlurScratch::new(), |_, _| 255);
        assert!(
            surface.pixels().is_empty(),
            "{w}x{h} has no pixels to frost"
        );
    }
}

#[test]
fn an_absurd_radius_saturates_rather_than_panicking() {
    // Radii past the point where a channel times the window's sample count
    // leaves `u32`: the arithmetic saturates instead of overflowing.
    for radius in [u32::MAX / 255, u32::MAX / 2, u32::MAX] {
        let surface = frosted(patterned(5, 5), (0, 0, 5, 5), radius, 255);
        assert!(
            surface
                .pixels()
                .iter()
                .all(|p| p.r <= p.a && p.g <= p.a && p.b <= p.a),
            "radius {radius} keeps the premultiplied invariant"
        );
    }
}

#[test]
fn a_reused_scratch_carries_nothing_between_frosts() {
    let mut warm = BlurScratch::new();
    let mut shared = patterned(9, 8);
    shared.frost_region(1, 1, 7, 5, 3, &mut warm, |_, _| 255);
    shared.frost_region(0, 2, 4, 3, 2, &mut warm, |_, _| 200);

    let mut fresh = patterned(9, 8);
    fresh.frost_region(1, 1, 7, 5, 3, &mut BlurScratch::new(), |_, _| 255);
    fresh.frost_region(0, 2, 4, 3, 2, &mut BlurScratch::new(), |_, _| 200);

    assert_eq!(shared, fresh, "a warm scratch frosts as a fresh one does");
}

#[test]
fn a_released_scratch_grows_again_and_frosts_the_same() {
    let mut scratch = BlurScratch::default();
    let mut first = patterned(8, 6);
    first.frost_region(0, 0, 8, 6, 3, &mut scratch, |_, _| 255);
    scratch.release();
    let mut second = patterned(8, 6);
    second.frost_region(0, 0, 8, 6, 3, &mut scratch, |_, _| 255);
    assert_eq!(first, second, "releasing the memory changes no result");
}
