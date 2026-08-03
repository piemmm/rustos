//! Unit tests for the shared desktop-icon library.

use alloc::vec;

use tairix_abi::blkio::BlkDeviceClass;
use tairix_raster::Color;

use crate::glyph::{builtin_icon, disk_icon, IconKind};
use crate::vector::{IconLayer, VectorIcon};

const FG: Color = Color::rgb(230, 230, 235);

/// The tests iterate the one canonical kind table rather than a second copy
/// (§2.2), so a new kind is covered the moment it enters `ICON_KINDS`.
const ALL_KINDS: [IconKind; crate::load::ICON_KINDS.len()] = crate::load::ICON_KINDS;

#[test]
fn index_is_the_position_in_the_kind_table() {
    for (position, kind) in ALL_KINDS.iter().enumerate() {
        assert_eq!(kind.index(), position, "{kind:?}");
        assert_eq!(ALL_KINDS[kind.index()], *kind, "{kind:?}");
    }
}

#[test]
fn for_asset_maps_known_ids() {
    assert_eq!(IconKind::for_asset("network"), IconKind::Network);
    assert_eq!(IconKind::for_asset("volume"), IconKind::Volume);
    assert_eq!(IconKind::for_asset("battery"), IconKind::Battery);
    assert_eq!(IconKind::for_asset("bell"), IconKind::Bell);
    assert_eq!(IconKind::for_asset("folder"), IconKind::Folder);
    assert_eq!(IconKind::for_asset("folder-open"), IconKind::FolderOpen);
    assert_eq!(IconKind::for_asset("file"), IconKind::File);
    assert_eq!(IconKind::for_asset("app-bundle"), IconKind::AppBundle);
    assert_eq!(IconKind::for_asset("text"), IconKind::Text);
    assert_eq!(IconKind::for_asset("image"), IconKind::Image);
    assert_eq!(IconKind::for_asset("archive"), IconKind::Archive);
    assert_eq!(IconKind::for_asset("executable"), IconKind::Executable);
    assert_eq!(IconKind::for_asset("nav-back"), IconKind::NavBack);
    assert_eq!(IconKind::for_asset("nav-forward"), IconKind::NavForward);
    assert_eq!(IconKind::for_asset("nav-up"), IconKind::NavUp);
    assert_eq!(IconKind::for_asset("refresh"), IconKind::Refresh);
    assert_eq!(IconKind::for_asset("view-toggle"), IconKind::ViewToggle);
    assert_eq!(IconKind::for_asset("sort"), IconKind::Sort);
    assert_eq!(IconKind::for_asset("new-folder"), IconKind::NewFolder);
    assert_eq!(IconKind::for_asset("trash"), IconKind::Trash);
    assert_eq!(IconKind::for_asset("empty-trash"), IconKind::EmptyTrash);
    assert_eq!(IconKind::for_asset("library"), IconKind::Library);
    assert_eq!(IconKind::for_asset("switchboard"), IconKind::Switchboard);
    assert_eq!(
        IconKind::for_asset("service-bundle"),
        IconKind::ServiceBundle
    );
    assert_eq!(IconKind::for_asset("text-html"), IconKind::TextHtml);
    assert_eq!(IconKind::for_asset("text-x-rust"), IconKind::TextRust);
    assert_eq!(IconKind::for_asset("text-x-java"), IconKind::TextJava);
    assert_eq!(
        IconKind::for_asset("application-x-shellscript"),
        IconKind::ShellScript
    );
    assert_eq!(IconKind::for_asset("application-pdf"), IconKind::Pdf);
    assert_eq!(IconKind::for_asset("image-png"), IconKind::ImagePng);
    assert_eq!(IconKind::for_asset("image-jpeg"), IconKind::ImageJpeg);
    assert_eq!(IconKind::for_asset("image-gif"), IconKind::ImageGif);
    assert_eq!(IconKind::for_asset("image-svg-xml"), IconKind::ImageSvg);
    assert_eq!(
        IconKind::for_asset("image-x-riscos-sprite"),
        IconKind::ImageSprite
    );
    assert_eq!(IconKind::for_asset("disk-hard"), IconKind::DiskHard);
    assert_eq!(
        IconKind::for_asset("disk-solid-state"),
        IconKind::DiskSolidState
    );
    assert_eq!(IconKind::for_asset("disk"), IconKind::Disk);
    assert_eq!(IconKind::for_asset("disk-usb"), IconKind::DiskUsb);
}

#[test]
fn the_disk_glyph_draws_pixels_for_every_disk_kind() {
    // The generic drive and the three fine-grained media share the one disk
    // glyph; each must rasterise to a legible, non-empty silhouette.
    for kind in [
        IconKind::Disk,
        IconKind::DiskHard,
        IconKind::DiskSolidState,
        IconKind::DiskUsb,
    ] {
        let image = builtin_icon(kind, FG).rasterise(16).expect("renderable");
        assert_eq!(image.width(), 16);
        assert_eq!(image.height(), 16);
        assert!(
            image.pixels().iter().filter(|p| p.a > 0).count() > 0,
            "{kind:?} drew nothing"
        );
    }
}

#[test]
fn a_fine_grained_kind_shares_its_family_glyph() {
    // The distinction is in the asset id, not the built-in fallback: a
    // fine-grained kind draws its broad family's glyph.
    assert_eq!(
        builtin_icon(IconKind::TextHtml, FG),
        builtin_icon(IconKind::Text, FG)
    );
    assert_eq!(
        builtin_icon(IconKind::ImagePng, FG),
        builtin_icon(IconKind::Image, FG)
    );
    assert_eq!(
        builtin_icon(IconKind::ServiceBundle, FG),
        builtin_icon(IconKind::AppBundle, FG)
    );
    assert_eq!(
        builtin_icon(IconKind::DiskHard, FG),
        builtin_icon(IconKind::DiskUsb, FG)
    );
}

#[test]
fn disk_icon_maps_every_medium() {
    assert_eq!(
        disk_icon(Some(BlkDeviceClass::Rotational)),
        IconKind::DiskHard
    );
    assert_eq!(
        disk_icon(Some(BlkDeviceClass::SolidState)),
        IconKind::DiskSolidState
    );
    assert_eq!(
        disk_icon(Some(BlkDeviceClass::Removable)),
        IconKind::DiskUsb
    );
    // A paravirtual device is not rotational, solid-state or removable, and
    // an unclassified mount is not known to be any of them: both draw the
    // generic drive rather than a guess.
    assert_eq!(disk_icon(Some(BlkDeviceClass::Virtual)), IconKind::Disk);
    assert_eq!(disk_icon(None), IconKind::Disk);
}

#[test]
fn for_asset_falls_back_to_generic() {
    assert_eq!(IconKind::for_asset(""), IconKind::Generic);
    assert_eq!(IconKind::for_asset("not-a-real-icon"), IconKind::Generic);
}

#[test]
fn asset_id_round_trips_through_for_asset() {
    for kind in ALL_KINDS {
        assert_eq!(IconKind::for_asset(kind.asset_id()), kind, "{kind:?}");
    }
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

#[test]
fn decodes_an_svg_icon_to_its_layers() {
    let svg = br##"<svg viewBox="0 0 24 24">
        <rect width="24" height="24" fill="#102030"/>
        <polygon points="4,4 20,4 12,20" fill="#ffaa00"/>
    </svg>"##;
    let icon = crate::decode_svg(svg).expect("valid svg icon");
    assert_eq!(icon.design(), 24);
    assert_eq!(icon.layers().len(), 2);
    assert_eq!(icon.layers()[0].fill, Color::rgb(0x10, 0x20, 0x30));
    assert_eq!(icon.layers()[1].polygon, vec![(4, 4), (20, 4), (12, 20)]);
}

#[test]
fn decoded_svg_icon_rasterises() {
    let svg =
        br##"<svg viewBox="0 0 16 16"><polygon points="2,2 14,2 14,14 2,14" fill="#3cf"/></svg>"##;
    let icon = crate::decode_svg(svg).expect("valid svg icon");
    let image = icon.rasterise(16).expect("renderable");
    assert!(image.pixels().iter().any(|p| p.a > 0));
}

#[test]
fn malformed_svg_icon_fails_closed() {
    // The caller substitutes a builtin glyph rather than crashing.
    assert!(crate::decode_svg(b"<not-svg/>").is_err());
}

/// A distinctive single-layer SVG icon whose authored fill (`#ffaa00`) no
/// built-in glyph uses, so a loaded asset is told apart from a tinted
/// fallback.
const LOADED_SVG: &[u8] =
    br##"<svg viewBox="0 0 24 24"><polygon points="4,4 20,4 12,20" fill="#ffaa00"/></svg>"##;

/// The authored fill of [`LOADED_SVG`].
const LOADED_FILL: Color = Color::rgb(0xff, 0xaa, 0x00);

/// An in-memory icon-asset source: returns [`LOADED_SVG`] for the listed
/// kinds and the given bytes for everything else (`None` by default).
struct TestSource {
    kinds: &'static [IconKind],
    other: Option<&'static [u8]>,
}

impl TestSource {
    const fn for_kinds(kinds: &'static [IconKind]) -> Self {
        Self { kinds, other: None }
    }
}

impl crate::IconAssetSource for TestSource {
    fn asset(&self, kind: IconKind) -> Option<&[u8]> {
        if self.kinds.contains(&kind) {
            Some(LOADED_SVG)
        } else {
            self.other
        }
    }
}

#[test]
fn icon_set_loads_every_kind_when_all_present() {
    let set = crate::IconSet::from_assets(&TestSource::for_kinds(&ALL_KINDS));
    for kind in ALL_KINDS {
        assert!(set.is_loaded(kind), "{kind:?} should be loaded");
        assert_eq!(
            set.icon(kind, FG).layers()[0].fill,
            LOADED_FILL,
            "{kind:?} kept its colours"
        );
    }
}

#[test]
fn icon_set_empty_source_falls_back_to_tinted_builtins() {
    let set = crate::IconSet::from_assets(&TestSource::for_kinds(&[]));
    for kind in ALL_KINDS {
        assert!(!set.is_loaded(kind), "{kind:?} should fall back");
        assert_eq!(set.icon(kind, FG), builtin_icon(kind, FG));
    }
}

#[test]
fn icon_set_mixes_loaded_assets_and_builtin_fallbacks() {
    let set = crate::IconSet::from_assets(&TestSource::for_kinds(&[IconKind::Bell]));
    assert!(set.is_loaded(IconKind::Bell));
    assert_eq!(set.icon(IconKind::Bell, FG).layers()[0].fill, LOADED_FILL);
    assert!(!set.is_loaded(IconKind::Network));
    assert_eq!(
        set.icon(IconKind::Network, FG),
        builtin_icon(IconKind::Network, FG)
    );
}

#[test]
fn icon_set_malformed_asset_falls_back_per_kind() {
    // Every kind is served broken bytes; each must fall back to its tinted
    // built-in glyph rather than failing.
    let source = TestSource {
        kinds: &[],
        other: Some(b"<not-svg/>"),
    };
    let set = crate::IconSet::from_assets(&source);
    for kind in ALL_KINDS {
        assert!(!set.is_loaded(kind));
        assert_eq!(set.icon(kind, FG), builtin_icon(kind, FG));
    }
}

#[test]
fn icon_set_ignores_tint_for_loaded_asset() {
    let set = crate::IconSet::from_assets(&TestSource::for_kinds(&[IconKind::Volume]));
    let red = Color::rgb(255, 0, 0);
    // The authored asset keeps its own colours regardless of the tint.
    assert_eq!(
        set.icon(IconKind::Volume, red).layers()[0].fill,
        LOADED_FILL
    );
}

#[test]
fn builtin_set_matches_an_empty_source_and_is_the_default() {
    // The const built-in set loads nothing, so every kind falls back to its
    // tinted built-in glyph — identical to building from an empty source.
    let builtin = crate::IconSet::builtin();
    let empty = crate::IconSet::from_assets(&TestSource::for_kinds(&[]));
    assert_eq!(builtin, empty);
    assert_eq!(builtin, crate::IconSet::default());
    for kind in ALL_KINDS {
        assert!(!builtin.is_loaded(kind));
        assert_eq!(builtin.icon(kind, FG), builtin_icon(kind, FG));
    }
}
