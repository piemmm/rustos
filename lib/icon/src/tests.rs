//! Unit tests for the shared desktop-icon library.

use alloc::vec;

use rustos_raster::Color;

use crate::glyph::{builtin_icon, IconKind};
use crate::vector::{IconLayer, VectorIcon};

const FG: Color = Color::rgb(230, 230, 235);

const ALL_KINDS: [IconKind; 5] = [
    IconKind::Network,
    IconKind::Volume,
    IconKind::Battery,
    IconKind::Bell,
    IconKind::Generic,
];

#[test]
fn for_asset_maps_known_ids() {
    assert_eq!(IconKind::for_asset("network"), IconKind::Network);
    assert_eq!(IconKind::for_asset("volume"), IconKind::Volume);
    assert_eq!(IconKind::for_asset("battery"), IconKind::Battery);
    assert_eq!(IconKind::for_asset("bell"), IconKind::Bell);
}

#[test]
fn for_asset_falls_back_to_generic() {
    assert_eq!(IconKind::for_asset(""), IconKind::Generic);
    assert_eq!(IconKind::for_asset("not-a-real-icon"), IconKind::Generic);
}

#[test]
fn every_builtin_glyph_draws_some_pixels() {
    for kind in ALL_KINDS {
        let icon = builtin_icon(kind, FG);
        let image = icon.rasterise(16).expect("renderable");
        assert_eq!(image.width(), 16);
        assert_eq!(image.height(), 16);
        let drawn = image.pixels().iter().filter(|p| p.a > 0).count();
        assert!(drawn > 0, "{kind:?} drew nothing");
    }
}

#[test]
fn glyph_is_tinted_by_the_requested_colour() {
    let red = Color::rgb(255, 0, 0);
    let icon = builtin_icon(IconKind::Battery, red);
    let image = icon.rasterise(24).expect("renderable");
    // Fully-covered interior pixels carry the requested colour; no green/blue.
    assert!(image
        .pixels()
        .iter()
        .any(|p| p.a == 255 && p.r == 255 && p.g == 0 && p.b == 0));
}

#[test]
fn rasterise_zero_side_is_none() {
    assert!(builtin_icon(IconKind::Bell, FG).rasterise(0).is_none());
}

#[test]
fn empty_icon_is_transparent() {
    let icon = VectorIcon::new(24, vec![]);
    let image = icon.rasterise(8).expect("renderable");
    assert!(image.pixels().iter().all(|p| p.a == 0));
    assert_eq!(icon.design(), 24);
    assert!(icon.layers().is_empty());
}

#[test]
fn scales_up_without_changing_the_grid() {
    let icon = builtin_icon(IconKind::Generic, FG);
    let small = icon.rasterise(12).expect("renderable");
    let big = icon.rasterise(24).expect("renderable");
    assert_eq!(big.width(), small.width() * 2);
    // The larger render covers proportionally more pixels.
    let small_drawn = small.pixels().iter().filter(|p| p.a > 0).count();
    let big_drawn = big.pixels().iter().filter(|p| p.a > 0).count();
    assert!(big_drawn > small_drawn);
}

#[test]
fn degenerate_layer_is_skipped() {
    let icon = VectorIcon::new(8, vec![IconLayer::from_points(FG, &[(0, 0), (8, 8)])]);
    let image = icon.rasterise(8).expect("renderable");
    assert!(image.pixels().iter().all(|p| p.a == 0));
}
