//! Unit tests for the shared image resampler.
//!
//! They pin down the properties every consumer relies on: an exact 1:1
//! resample is a copy, a reduction is a true area average weighted by real
//! coverage (so it blends rather than aliases, at ratios that are not whole
//! numbers as much as at ratios that are), an enlargement interpolates
//! instead of reproducing the source grid as blocks, a flat region survives
//! exactly, alpha never bleeds, and a band is byte-for-byte the slice of the
//! whole image it claims to be. The rest are the fail-closed refusals.

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

/// A `width`×1 opaque greyscale strip from `levels`.
fn strip(levels: &[u8]) -> Vec<u8> {
    levels
        .iter()
        .flat_map(|&level| [level, level, level, 255])
        .collect()
}

/// The red channel of every pixel of a `width`-wide row-major image row.
fn reds(pixels: &[u8], width: usize, row: usize) -> Vec<u8> {
    (0..width)
        .map(|x| pixels[((row * width) + x) * 4])
        .collect()
}

#[test]
fn a_one_to_one_resample_is_an_exact_copy() {
    let pixels = quad();
    let src = Rgba8Image::new(2, 2, &pixels).expect("well-formed");
    let out = resample(&src, src.whole(), 2, 2).expect("resampled");
    assert_eq!(out, pixels);
}

#[test]
fn a_one_to_one_resample_of_a_sub_region_is_that_region_exactly() {
    // The 1:1 case is not only the whole image: a crop drawn at its own
    // size is one too, and it must answer the cropped pixels rather than
    // the image's leading ones. Alpha varies across the region, because
    // reproducing a partly transparent pixel exactly is the property a
    // premultiplying filter has to round-trip to reach.
    let pixels = vec![
        1, 2, 3, 255, 4, 5, 6, 128, 7, 8, 9, 0, //
        10, 11, 12, 64, 13, 14, 15, 255, 16, 17, 18, 200, //
        19, 20, 21, 7, 22, 23, 24, 255, 25, 26, 27, 255,
    ];
    let src = Rgba8Image::new(3, 3, &pixels).expect("well-formed");
    let region = Region {
        x: 1,
        y: 1,
        width: 2,
        height: 2,
    };
    let out = resample(&src, region, 2, 2).expect("resampled");
    assert_eq!(
        out,
        vec![13, 14, 15, 255, 16, 17, 18, 200, 22, 23, 24, 255, 25, 26, 27, 255]
    );
}

#[test]
fn a_one_to_one_band_is_the_slice_of_the_copy_it_claims_to_be() {
    // Bands must stay independent of each other for the 1:1 case exactly
    // as they are for a filtered one: the desktop assembles a wallpaper
    // from them, and a band that read the wrong source row would tear the
    // picture at every band boundary.
    let pixels: Vec<u8> = (0..4 * 5 * 4)
        .map(|i| u8::try_from(i % 256).unwrap_or(0))
        .collect();
    let src = Rgba8Image::new(4, 5, &pixels).expect("well-formed");
    let whole = resample(&src, src.whole(), 4, 5).expect("resampled");
    let mut assembled = vec![0u8; whole.len()];
    for first_row in 0..5 {
        let row = &mut assembled[first_row * 16..(first_row + 1) * 16];
        resample_rows(
            &src,
            src.whole(),
            4,
            5,
            u32::try_from(first_row).expect("small"),
            1,
            row,
        )
        .expect("banded");
    }
    assert_eq!(assembled, whole);
    assert_eq!(assembled, pixels);
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
fn a_reduction_weights_a_partly_covered_source_pixel_by_its_coverage() {
    // Three source samples into two destination samples: each destination
    // sample covers one and a half source samples, so the middle sample is
    // split evenly between them. A filter that took whole samples would
    // answer 0 and 120, or 60 and 120 — either way, one of the two would
    // ignore a sample it half covers, and that is the aliasing this filter
    // exists to avoid.
    let pixels = strip(&[0, 60, 120]);
    let src = Rgba8Image::new(3, 1, &pixels).expect("well-formed");
    let out = resample(&src, src.whole(), 2, 1).expect("resampled");
    // (0 * 1 + 60 * 0.5) / 1.5 = 20, and (60 * 0.5 + 120 * 1) / 1.5 = 100.
    assert_eq!(reds(&out, 2, 0), vec![20, 100]);
}

#[test]
fn an_enlargement_interpolates_rather_than_reproducing_the_source_grid() {
    // Four samples to eight: a sample-and-hold would answer each source
    // level twice, so adjacent destination pixels would be equal. Strictly
    // rising is therefore the exact statement that no source sample was
    // held — the visible blockiness an enlarged wallpaper must not show.
    let pixels = strip(&[0, 60, 120, 180]);
    let src = Rgba8Image::new(4, 1, &pixels).expect("well-formed");
    let out = resample(&src, src.whole(), 8, 1).expect("resampled");
    let got = reds(&out, 8, 0);
    assert!(
        got.windows(2).all(|pair| pair[0] < pair[1]),
        "strictly rising: {got:?}"
    );
}

#[test]
fn an_enlargement_keeps_a_flat_region_flat() {
    // The cubic's negative lobes must cancel exactly on a flat source:
    // weights that summed to anything but one would show as banding.
    let pixels = flat(3, 3, [37, 211, 88, 255]);
    let src = Rgba8Image::new(3, 3, &pixels).expect("well-formed");
    let out = resample(&src, src.whole(), 17, 11).expect("resampled");
    assert!(
        out.as_chunks::<4>()
            .0
            .iter()
            .all(|px| *px == [37, 211, 88, 255]),
        "an enlarged flat region is still flat"
    );
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
fn a_region_enlarges_from_its_own_edge_samples_alone() {
    // The cubic reaches a sample either side of its footprint. At a region
    // boundary that neighbour lies outside the region, and holding the
    // region's own edge sample there is what stops a crop from dragging in
    // pixels the caller excluded.
    let pixels = strip(&[0, 255, 255, 0]);
    let src = Rgba8Image::new(4, 1, &pixels).expect("well-formed");
    let middle = Region {
        x: 1,
        y: 0,
        width: 2,
        height: 1,
    };
    let out = resample(&src, middle, 6, 1).expect("resampled");
    assert!(
        reds(&out, 6, 0).iter().all(|&value| value == 255),
        "the excluded black neighbours never contribute"
    );
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
    for (dw, dh) in [(5u32, 4u32), (23u32, 19u32)] {
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
            assert_eq!(assembled, whole, "{dw}x{dh} in bands of {band} row(s)");
        }
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
fn an_enlarged_alpha_edge_keeps_its_colour_across_the_ramp() {
    // Enlarging an opaque-to-transparent edge: alpha ramps, but the colour
    // must stay the opaque pixel's colour rather than being dragged toward
    // the transparent pixel's meaningless one.
    let pixels = vec![10, 200, 30, 255, 90, 90, 90, 0];
    let src = Rgba8Image::new(2, 1, &pixels).expect("well-formed");
    let out = resample(&src, src.whole(), 6, 1).expect("resampled");
    let (pixels, _tail) = out.as_chunks::<4>();
    for pixel in pixels.iter().filter(|px| px[3] > 0) {
        assert_eq!([pixel[0], pixel[1], pixel[2]], [10, 200, 30], "{pixel:?}");
    }
    assert!(
        pixels.windows(2).all(|pair| pair[0][3] >= pair[1][3]),
        "alpha falls across the ramp: {pixels:?}"
    );
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
