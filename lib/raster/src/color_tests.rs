//! Unit tests for the span composite.
//!
//! The per-pixel operators are exercised throughout the crate's own tests and
//! by every caller; what needs proving here is that laying a *run* of them is
//! the same arithmetic, and that where a run starts cannot change what it
//! writes.

use alloc::vec::Vec;

use tairix_rng::RandU64;

use super::{blend_span, Pixel};
use crate::dither::DitherRow;

/// A deterministic stream of premultiplied pixels, so a failure is
/// reproducible from the seed alone. `lib/rng`'s own fast generator, as the
/// blur tests use, rather than a second one written here.
struct Pixels(tairix_rng::FastRng);

impl Pixels {
    fn new(seed: u64) -> Self {
        Self(tairix_rng::FastRng::seed_from_u64(seed))
    }

    /// Alpha first, then channels that cannot exceed it, which is the
    /// premultiplied invariant every operator here relies on.
    fn next(&mut self) -> Pixel {
        let bytes = self.0.next_u64().to_le_bytes();
        let a = bytes[3];
        Pixel {
            r: bytes[0].min(a),
            g: bytes[1].min(a),
            b: bytes[2].min(a),
            a,
        }
    }

    fn run(&mut self, len: usize) -> Vec<Pixel> {
        (0..len).map(|_| self.next()).collect()
    }
}

/// What the span blend replaces: the same source, scaled and composited one
/// pixel at a time at that pixel's own bias.
fn pixel_by_pixel(
    dst: &[Pixel],
    src: &[Pixel],
    factor: u8,
    dither: DitherRow,
    first_x: u32,
) -> Vec<Pixel> {
    dst.iter()
        .zip(src)
        .zip(first_x..)
        .map(|((dst, src), x)| {
            if src.a == 0 {
                return *dst;
            }
            let bias = dither.bias(x);
            src.scale_alpha_biased(factor, bias).over_biased(*dst, bias)
        })
        .collect()
}

#[test]
fn a_blended_run_is_exactly_the_pixels_blended_one_at_a_time() {
    let mut rng = Pixels::new(0x5EED_0C01_0700_51DE);
    for row in 0..8 {
        let dither = DitherRow::at(row);
        for factor in [255u8, 200, 160, 128, 1, 0] {
            for first_x in [0u32, 1, 7, 8, 63, 4096] {
                let (before, src) = (rng.run(37), rng.run(37));
                let mut after = before.clone();
                blend_span(&mut after, &src, factor, dither, first_x);
                assert_eq!(
                    after,
                    pixel_by_pixel(&before, &src, factor, dither, first_x),
                    "row {row}, factor {factor}, from column {first_x}"
                );
            }
        }
    }
}

#[test]
fn every_length_across_the_tile_boundary_matches_the_reference() {
    // The walk composites whole eight-pixel tiles and then the remainder, so
    // a run of any length beginning at any phase must still be the pixels
    // blended one at a time: a remainder that read the tile from lane zero,
    // or a tile boundary that reset the phase, would show here.
    let mut rng = Pixels::new(0x711E_0B0D_1E5A_2C10);
    let dither = DitherRow::at(5);
    for len in 0..=24usize {
        for first_x in 0..=8u32 {
            let (before, src) = (rng.run(len), rng.run(len));
            let mut after = before.clone();
            blend_span(&mut after, &src, 192, dither, first_x);
            assert_eq!(
                after,
                pixel_by_pixel(&before, &src, 192, dither, first_x),
                "length {len} from column {first_x}"
            );
        }
    }
}

#[test]
fn a_run_split_in_two_writes_what_the_whole_run_wrote() {
    // The compositor lays a row in segments whose boundaries move with the
    // windows on it. A span that read its dither from its own start would put
    // a different pattern either side of a boundary that has nothing to do
    // with the picture, and the join would be visible.
    let mut rng = Pixels::new(0x00B0_1171_DE0F_0ED0);
    let dither = DitherRow::at(3);
    let (before, src) = (rng.run(40), rng.run(40));

    let mut whole = before.clone();
    blend_span(&mut whole, &src, 176, dither, 5);

    for split in 0..=40 {
        let mut pieced = before.clone();
        let (left, right) = pieced.split_at_mut(split);
        blend_span(left, &src[..split], 176, dither, 5);
        blend_span(
            right,
            &src[split..],
            176,
            dither,
            5 + u32::try_from(split).expect("a small index"),
        );
        assert_eq!(pieced, whole, "split at {split}");
    }
}

#[test]
fn a_transparent_source_pixel_leaves_its_destination_exactly_as_it_was() {
    let mut rng = Pixels::new(0x0000_0000_C1EA_2000);
    let before = rng.run(16);
    let src = [Pixel::TRANSPARENT; 16];
    let mut after = before.clone();
    blend_span(&mut after, &src, 200, DitherRow::at(1), 0);
    assert_eq!(after, before);
}

#[test]
fn the_shorter_of_the_two_runs_ends_the_walk() {
    let mut rng = Pixels::new(0x1234_5678_9ABC_DEF0);
    let before = rng.run(8);
    let src = rng.run(3);
    let mut after = before.clone();
    blend_span(&mut after, &src, 255, DitherRow::at(0), 0);
    assert_eq!(
        after.get(3..),
        before.get(3..),
        "no source, no write — a short run does not wrap or repeat"
    );
}
