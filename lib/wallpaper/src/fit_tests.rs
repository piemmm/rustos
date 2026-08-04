//! Unit tests for the wallpaper placement geometry.

use super::*;
use tairix_geometry::Rect;

const LANDSCAPE_SCREEN: (u32, u32) = (1920, 1080);
const PORTRAIT_SOURCE: (u32, u32) = (1080, 1920);
const LANDSCAPE_SOURCE: (u32, u32) = (3840, 2160);
const SQUARE: (u32, u32) = (512, 512);

#[test]
fn degenerate_source_or_screen_yields_no_placement() {
    assert_eq!(place((0, 100), (10, 10), WallpaperFit::Fill), None);
    assert_eq!(place((100, 0), (10, 10), WallpaperFit::Fill), None);
    assert_eq!(place((10, 10), (0, 100), WallpaperFit::Fill), None);
    assert_eq!(place((10, 10), (100, 0), WallpaperFit::Fill), None);
    for fit in [
        WallpaperFit::Fill,
        WallpaperFit::Fit,
        WallpaperFit::Stretch,
        WallpaperFit::Centre,
        WallpaperFit::Tile,
    ] {
        assert_eq!(place((0, 0), (0, 0), fit), None);
    }
}

#[test]
fn stretch_always_fills_the_screen_with_the_whole_source() {
    for (source, screen) in [
        (LANDSCAPE_SOURCE, LANDSCAPE_SCREEN),
        (PORTRAIT_SOURCE, LANDSCAPE_SCREEN),
        (SQUARE, LANDSCAPE_SCREEN),
        (SQUARE, SQUARE),
        ((1, 1), (1, 1)),
        ((1, 1_000_000), (1_000_000, 1)),
    ] {
        let placement = place(source, screen, WallpaperFit::Stretch).unwrap();
        assert_eq!(placement.destination(), Rect::new(0, 0, screen.0, screen.1));
        assert_eq!(placement.source(), Rect::new(0, 0, source.0, source.1));
        assert!(!placement.tiled());
    }
}

#[test]
fn tile_always_repeats_the_full_source_across_the_screen() {
    for (source, screen) in [
        (LANDSCAPE_SOURCE, LANDSCAPE_SCREEN),
        (PORTRAIT_SOURCE, LANDSCAPE_SCREEN),
        (SQUARE, SQUARE),
        ((1, 1), (4000, 3000)),
    ] {
        let placement = place(source, screen, WallpaperFit::Tile).unwrap();
        assert_eq!(placement.destination(), Rect::new(0, 0, screen.0, screen.1));
        assert_eq!(placement.source(), Rect::new(0, 0, source.0, source.1));
        assert!(placement.tiled());
    }
}

#[test]
fn fill_on_a_square_source_and_landscape_screen_crops_height() {
    // A square source is "relatively taller" than a landscape screen, so
    // the full width is kept and the height is cropped, centred.
    let placement = place(SQUARE, LANDSCAPE_SCREEN, WallpaperFit::Fill).unwrap();
    assert_eq!(
        placement.destination(),
        Rect::new(0, 0, LANDSCAPE_SCREEN.0, LANDSCAPE_SCREEN.1)
    );
    // cropped_h = src_w * screen_h / screen_w = 512 * 1080 / 1920 = 288.
    let expected_h = 288;
    let expected_y = (SQUARE.1 - expected_h) / 2;
    assert_eq!(
        placement.source(),
        Rect::new(0, i32::try_from(expected_y).unwrap(), SQUARE.0, expected_h)
    );
    assert!(!placement.tiled());
}

#[test]
fn fill_on_a_wide_source_and_square_screen_crops_width() {
    let source = (1000, 100);
    let screen = (100, 100);
    let placement = place(source, screen, WallpaperFit::Fill).unwrap();
    assert_eq!(placement.destination(), Rect::new(0, 0, 100, 100));
    // cropped_w = src_h * screen_w / screen_h = 100 * 100 / 100 = 100.
    assert_eq!(placement.source(), Rect::new(450, 0, 100, 100));
}

#[test]
fn fill_on_matching_aspect_ratios_needs_no_crop() {
    let placement = place((1920, 1080), (960, 540), WallpaperFit::Fill).unwrap();
    assert_eq!(placement.destination(), Rect::new(0, 0, 960, 540));
    assert_eq!(placement.source(), Rect::new(0, 0, 1920, 1080));
}

#[test]
fn fill_on_a_1x1_source_and_screen_is_exact() {
    let placement = place((1, 1), (1, 1), WallpaperFit::Fill).unwrap();
    assert_eq!(placement.destination(), Rect::new(0, 0, 1, 1));
    assert_eq!(placement.source(), Rect::new(0, 0, 1, 1));
}

#[test]
fn fill_at_an_extreme_aspect_ratio_never_panics_and_stays_nonempty() {
    let placement = place((1, 1_000_000), (1_000_000, 1), WallpaperFit::Fill).unwrap();
    assert_eq!(placement.destination(), Rect::new(0, 0, 1_000_000, 1));
    // The crop clamps to at least one source pixel rather than degenerating
    // to an empty rectangle.
    assert!(!placement.source().is_empty());
    assert_eq!(placement.source().width, 1);
    assert_eq!(placement.source().height, 1);

    let placement = place((1_000_000, 1), (1, 1_000_000), WallpaperFit::Fill).unwrap();
    assert_eq!(placement.destination(), Rect::new(0, 0, 1, 1_000_000));
    assert!(!placement.source().is_empty());
}

#[test]
fn fit_on_a_square_source_and_landscape_screen_letterboxes_the_sides() {
    // A square source is "relatively taller": height is the tighter
    // constraint, so the full screen height is used and the width is
    // scaled down, letterboxed left/right.
    let placement = place(SQUARE, LANDSCAPE_SCREEN, WallpaperFit::Fit).unwrap();
    assert_eq!(placement.source(), Rect::new(0, 0, SQUARE.0, SQUARE.1));
    // dst_w = src_w * screen_h / src_h = 512 * 1080 / 512 = 1080.
    let expected_w = 1080;
    let expected_x = (LANDSCAPE_SCREEN.0 - expected_w) / 2;
    assert_eq!(
        placement.destination(),
        Rect::new(
            i32::try_from(expected_x).unwrap(),
            0,
            expected_w,
            LANDSCAPE_SCREEN.1
        )
    );
    assert!(!placement.tiled());
}

#[test]
fn fit_on_a_wide_source_and_square_screen_letterboxes_top_and_bottom() {
    let source = (1000, 100);
    let screen = (100, 100);
    let placement = place(source, screen, WallpaperFit::Fit).unwrap();
    assert_eq!(placement.source(), Rect::new(0, 0, 1000, 100));
    // dst_h = src_h * screen_w / src_w = 100 * 100 / 1000 = 10.
    assert_eq!(placement.destination(), Rect::new(0, 45, 100, 10));
}

#[test]
fn fit_on_matching_aspect_ratios_fills_exactly() {
    let placement = place((1920, 1080), (960, 540), WallpaperFit::Fit).unwrap();
    assert_eq!(placement.destination(), Rect::new(0, 0, 960, 540));
    assert_eq!(placement.source(), Rect::new(0, 0, 1920, 1080));
}

#[test]
fn fit_at_an_extreme_aspect_ratio_never_panics_and_stays_nonempty() {
    let placement = place((1, 1_000_000), (1_000_000, 1), WallpaperFit::Fit).unwrap();
    assert!(!placement.destination().is_empty());
    assert_eq!(placement.source(), Rect::new(0, 0, 1, 1_000_000));
}

#[test]
fn centre_with_a_source_smaller_than_the_screen_is_1_to_1_and_uncropped() {
    let placement = place((200, 100), (1920, 1080), WallpaperFit::Centre).unwrap();
    assert_eq!(placement.source(), Rect::new(0, 0, 200, 100));
    assert_eq!(placement.destination(), Rect::new(860, 490, 200, 100));
    assert!(!placement.tiled());
}

#[test]
fn centre_with_a_source_larger_than_the_screen_crops_both_dimensions() {
    let placement = place((3840, 2160), (1920, 1080), WallpaperFit::Centre).unwrap();
    // Screen is exactly half the source's size in both dimensions.
    assert_eq!(placement.destination(), Rect::new(0, 0, 1920, 1080));
    assert_eq!(placement.source(), Rect::new(960, 540, 1920, 1080));
}

#[test]
fn centre_with_a_source_larger_in_only_one_dimension() {
    // Source wider than the screen, but not taller.
    let placement = place((4000, 100), (1920, 1080), WallpaperFit::Centre).unwrap();
    assert_eq!(placement.destination(), Rect::new(0, 490, 1920, 100));
    assert_eq!(placement.source(), Rect::new(1040, 0, 1920, 100));
}

#[test]
fn centre_on_equal_1x1_sizes_is_exact() {
    let placement = place((1, 1), (1, 1), WallpaperFit::Centre).unwrap();
    assert_eq!(placement.destination(), Rect::new(0, 0, 1, 1));
    assert_eq!(placement.source(), Rect::new(0, 0, 1, 1));
}

#[test]
fn decode_target_for_fill_is_the_smaller_of_crop_and_destination() {
    // Downscale case: cropped source region is larger than the destination
    // (the screen), so the decoder need only produce screen-sized pixels.
    let target = decode_target((3840, 2160), (960, 540), WallpaperFit::Fill).unwrap();
    assert_eq!(target, (960, 540));

    // Upscale case: the source is smaller than the screen, so the decoder
    // can never be asked for more than the source's own native size.
    let target = decode_target((100, 100), (1920, 1080), WallpaperFit::Fill).unwrap();
    let placement = place((100, 100), (1920, 1080), WallpaperFit::Fill).unwrap();
    assert_eq!(
        target,
        (placement.source().width, placement.source().height)
    );
}

#[test]
fn decode_target_for_tile_is_always_the_full_native_source() {
    let target = decode_target((64, 64), (1920, 1080), WallpaperFit::Tile).unwrap();
    assert_eq!(target, (64, 64));
}

#[test]
fn decode_target_returns_none_exactly_when_place_does() {
    assert_eq!(decode_target((0, 100), (10, 10), WallpaperFit::Fit), None);
    assert!(decode_target((10, 10), (10, 10), WallpaperFit::Fit).is_some());
}

#[test]
fn decode_target_never_exceeds_the_destination_size() {
    for fit in [
        WallpaperFit::Fill,
        WallpaperFit::Fit,
        WallpaperFit::Stretch,
        WallpaperFit::Centre,
    ] {
        let target = decode_target(LANDSCAPE_SOURCE, (320, 200), fit).unwrap();
        assert!(target.0 <= 320, "{fit:?}: {target:?}");
        assert!(target.1 <= 200, "{fit:?}: {target:?}");
    }
}
