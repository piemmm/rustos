//! Unit tests for the shared image resampler.
//!
//! They pin down the three properties every consumer relies on: an exact
//! 1:1 resample is a copy, a downscale is a true area average (so it blends
//! rather than aliases), and a band is byte-for-byte the slice of the whole
//! image it claims to be. The rest are the fail-closed refusals.

use alloc::vec;
use alloc::vec::Vec;

use super::{resample, resample_rows, Region, ResampleError, Rgba8Image};

/// A `width`×`height` image whose every pixel is `pixel`.
fn flat(width: u32, height: u32, pixel: [u8; 4]) -> Vec<u8> {
    pixel
        .iter()
        .copied()
        .cycle()
        .take(width as usize * height as usize * 4)
        .collect()
}

/// A 2×2 opaque image of four distinct greys, laid out row-major.
fn quad() -> Vec<u8> {
    vec![
        0, 0, 0, 255, // top left
        60, 60, 60, 255, // top right
        120, 120, 120, 255, // bottom left
        180, 180, 180, 255, // bottom right
    ]
}

#[test]
fn a_one_to_one_resample_is_an_exact_copy() {
    let pixels = quad();
    let src = Rgba8Image::new(2, 2, &pixels).expect("well-formed");
    let out = resample(&src, src.whole(), 2, 2).expect("resampled");
    assert_eq!(out, pixels);
}

#[test]
fn a_halving_downscale_averages_every_source_pixel_it_covers() {
    let pixels = quad();
    let src = Rgba8Image::new(2, 2, &pixels).expect("well-formed");
    let out = resample(&src, src.whole(), 1, 1).expect("resampled");
    // (0 + 60 + 120 + 180) / 4 = 90, and the alpha is unchanged.
    assert_eq!(out, vec![90, 90, 90, 255]);
}

#[test]
fn an_upscale_holds_each_source_sample_rather_than_inventing_detail() {
    let pixels = quad();
    let src = Rgba8Image::new(2, 2, &pixels).expect("well-formed");
    let out = resample(&src, src.whole(), 4, 4).expect("resampled");
    // Every 2×2 block of the destination is one source pixel, unblended.
    let at = |x: usize, y: usize| out[(y * 4 + x) * 4];
    for (x, y) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
        assert_eq!(at(x, y), 0, "top-left block");
        assert_eq!(at(x + 2, y), 60, "top-right block");
        assert_eq!(at(x, y + 2), 120, "bottom-left block");
        assert_eq!(at(x + 2, y + 2), 180, "bottom-right block");
    }
}

#[test]
fn a_region_resamples_only_the_pixels_inside_it() {
    let pixels = quad();
    let src = Rgba8Image::new(2, 2, &pixels).expect("well-formed");
    let bottom_right = Region {
        x: 1,
        y: 1,
        width: 1,
        height: 1,
    };
    let out = resample(&src, bottom_right, 1, 1).expect("resampled");
    assert_eq!(out, vec![180, 180, 180, 255]);
}

#[test]
fn bands_reassemble_into_exactly_the_whole_image() {
    // A gradient large enough that a naive per-band recomputation would drift.
    let (w, h) = (9u32, 7u32);
    let mut pixels = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let v = u8::try_from((x * 7 + y * 11) % 256).expect("bounded");
            pixels.extend_from_slice(&[v, v / 2, 255 - v, 255]);
        }
    }
    let src = Rgba8Image::new(w, h, &pixels).expect("well-formed");
    let (dw, dh) = (5u32, 4u32);
    let whole = resample(&src, src.whole(), dw, dh).expect("resampled");

    for band in 1..=dh {
        let mut assembled = Vec::new();
        let mut first = 0;
        while first < dh {
            let rows = band.min(dh - first);
            let mut chunk = vec![0u8; rows as usize * dw as usize * 4];
            resample_rows(&src, src.whole(), dw, dh, first, rows, &mut chunk)
                .expect("band resampled");
            assembled.extend_from_slice(&chunk);
            first += rows;
        }
        assert_eq!(assembled, whole, "bands of {band} row(s)");
    }
}

#[test]
fn transparent_padding_never_bleeds_its_colour_into_the_average() {
    // A red pixel beside a fully transparent white one: the average must be
    // red at half alpha, not a washed-out pink.
    let pixels = vec![255, 0, 0, 255, 255, 255, 255, 0];
    let src = Rgba8Image::new(2, 1, &pixels).expect("well-formed");
    let out = resample(&src, src.whole(), 1, 1).expect("resampled");
    assert_eq!(out, vec![255, 0, 0, 128]);
}

#[test]
fn a_fully_transparent_box_has_no_colour_to_report() {
    let pixels = flat(2, 2, [200, 100, 50, 0]);
    let src = Rgba8Image::new(2, 2, &pixels).expect("well-formed");
    let out = resample(&src, src.whole(), 1, 1).expect("resampled");
    assert_eq!(out, vec![0, 0, 0, 0]);
}

#[test]
fn an_extreme_aspect_change_stays_total_and_exact() {
    let pixels = flat(1, 1024, [10, 20, 30, 255]);
    let src = Rgba8Image::new(1, 1024, &pixels).expect("well-formed");
    let out = resample(&src, src.whole(), 1024, 1).expect("resampled");
    assert_eq!(out.len(), 1024 * 4);
    assert!(out
        .as_chunks::<4>()
        .0
        .iter()
        .all(|px| *px == [10, 20, 30, 255]));
}

#[test]
fn a_declared_size_that_does_not_match_the_pixels_is_refused() {
    let pixels = vec![0u8; 15];
    assert_eq!(
        Rgba8Image::new(2, 2, &pixels).unwrap_err(),
        ResampleError::SourceSizeMismatch
    );
    assert_eq!(
        Rgba8Image::new(0, 2, &[]).unwrap_err(),
        ResampleError::SourceSizeMismatch
    );
}

#[test]
fn a_region_outside_the_source_is_refused() {
    let pixels = quad();
    let src = Rgba8Image::new(2, 2, &pixels).expect("well-formed");
    for region in [
        Region {
            x: 0,
            y: 0,
            width: 0,
            height: 1,
        },
        Region {
            x: 2,
            y: 0,
            width: 1,
            height: 1,
        },
        Region {
            x: 0,
            y: 0,
            width: 3,
            height: 1,
        },
        Region {
            x: u32::MAX,
            y: 0,
            width: 2,
            height: 1,
        },
    ] {
        assert_eq!(
            resample(&src, region, 1, 1).unwrap_err(),
            ResampleError::SourceRegionOutOfBounds,
            "{region:?}"
        );
    }
}

#[test]
fn a_degenerate_destination_and_a_bad_band_are_refused() {
    let pixels = quad();
    let src = Rgba8Image::new(2, 2, &pixels).expect("well-formed");
    assert_eq!(
        resample(&src, src.whole(), 0, 4).unwrap_err(),
        ResampleError::EmptyDestination
    );
    let mut out = vec![0u8; 16];
    assert_eq!(
        resample_rows(&src, src.whole(), 2, 2, 0, 0, &mut out).unwrap_err(),
        ResampleError::BandOutOfBounds,
        "an empty band"
    );
    assert_eq!(
        resample_rows(&src, src.whole(), 2, 2, 1, 2, &mut out).unwrap_err(),
        ResampleError::BandOutOfBounds,
        "a band running past the last row"
    );
    assert_eq!(
        resample_rows(&src, src.whole(), 2, 2, u32::MAX, 2, &mut out).unwrap_err(),
        ResampleError::BandOutOfBounds,
        "a band whose end overflows"
    );
}

#[test]
fn a_mis_sized_output_buffer_is_refused_before_a_byte_is_written() {
    let pixels = quad();
    let src = Rgba8Image::new(2, 2, &pixels).expect("well-formed");
    let mut out = vec![0xAAu8; 15];
    assert_eq!(
        resample_rows(&src, src.whole(), 2, 2, 0, 2, &mut out).unwrap_err(),
        ResampleError::OutputSizeMismatch
    );
    assert!(out.iter().all(|&b| b == 0xAA), "nothing was written");
}
