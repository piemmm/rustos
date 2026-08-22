//! Unit tests for the cursor library: scaling, anti-aliased fill, colour,
//! hotspots, and the replaceable cursor-set registry.

use alloc::vec;

use tairix_raster::{Color, Paint};
use tairix_theme::{CursorKind, CURSOR_KINDS};

use crate::registry::{CursorRegistry, CursorRegistryError, CursorSetId};
use crate::theme::CursorTheme;
use crate::vector::{Shape, VectorCursor, Vertex};

/// The colour a shape paints with. Every built-in cursor and every test
/// asset here is a flat fill, so anything else is a broken expectation.
#[track_caller]
fn solid(shape: &Shape) -> Color {
    match shape.paint {
        Paint::Solid(color) => color,
        Paint::Gradient(_) => panic!("expected a flat fill"),
    }
}

/// Where the test asset's `(1, 2)` hotspot lands once its twenty-four-unit
/// drawing is scaled onto the decoder's shared design grid.
const HOTSPOT: (i32, i32) = (85, 171);

/// An opaque square cursor filling its whole `size`×`size` design grid.
fn solid_square(size: u32, fill: Color) -> VectorCursor {
    let s = i32::try_from(size).unwrap_or(i32::MAX);
    let shape = Shape::new(
        fill,
        vec![
            Vertex::new(0, 0),
            Vertex::new(s, 0),
            Vertex::new(s, s),
            Vertex::new(0, s),
        ],
    );
    VectorCursor::new(size, 0, 0, vec![shape])
}

#[test]
fn footprint_scales_with_percent() {
    let cursor = solid_square(32, Color::rgb(255, 255, 255));
    assert_eq!(cursor.footprint(100), Some(32));
    assert_eq!(cursor.footprint(200), Some(64));
    assert_eq!(cursor.footprint(50), Some(16));
}

#[test]
fn footprint_zero_scale_is_unrenderable() {
    let cursor = solid_square(32, Color::rgb(255, 255, 255));
    assert_eq!(cursor.footprint(0), None);
    assert!(cursor.rasterise(0).is_none());
}

#[test]
fn empty_design_grid_is_unrenderable() {
    let cursor = solid_square(0, Color::rgb(255, 255, 255));
    assert_eq!(cursor.footprint(100), None);
    assert!(cursor.rasterise(100).is_none());
}

#[test]
fn rasterised_image_is_square_at_scale() {
    let cursor = solid_square(8, Color::rgb(255, 255, 255));
    let image = cursor.rasterise(100).expect("renderable");
    assert_eq!(image.width(), 8);
    assert_eq!(image.height(), 8);

    let big = cursor.rasterise(300).expect("renderable");
    assert_eq!(big.width(), 24);
    assert_eq!(big.height(), 24);
}

#[test]
fn solid_square_fills_every_pixel_opaque() {
    let cursor = solid_square(4, Color::rgb(200, 100, 50));
    let image = cursor.rasterise(100).expect("renderable");
    let surface = image.surface();
    for y in 0..surface.height() {
        for x in 0..surface.width() {
            let pixel = surface.get(x, y).expect("in bounds");
            assert_eq!(pixel.a, 255, "pixel ({x},{y}) should be opaque");
            let colour = pixel.unpremultiply();
            assert_eq!((colour.r, colour.g, colour.b), (200, 100, 50));
        }
    }
}

#[test]
fn shape_with_fewer_than_three_vertices_is_skipped() {
    let degenerate = Shape::new(
        Color::rgb(255, 0, 0),
        vec![Vertex::new(0, 0), Vertex::new(4, 4)],
    );
    let cursor = VectorCursor::new(4, 0, 0, vec![degenerate]);
    let image = cursor.rasterise(100).expect("renderable");
    let surface = image.surface();
    for y in 0..surface.height() {
        for x in 0..surface.width() {
            assert_eq!(surface.get(x, y).expect("in bounds").a, 0);
        }
    }
}

#[test]
fn translucent_fill_blends_rather_than_overwrites() {
    let shape = Shape::new(
        Color::rgba(255, 255, 255, 128),
        vec![
            Vertex::new(0, 0),
            Vertex::new(4, 0),
            Vertex::new(4, 4),
            Vertex::new(0, 4),
        ],
    );
    let cursor = VectorCursor::new(4, 0, 0, vec![shape]);
    let image = cursor.rasterise(100).expect("renderable");
    let pixel = image.surface().get(2, 2).expect("in bounds");
    assert_eq!(pixel.a, 128, "half-transparent fill stays half transparent");
}

#[test]
fn builtin_set_defines_every_kind_renderable() {
    let theme = CursorTheme::builtin();
    for kind in CURSOR_KINDS {
        let cursor = theme.cursor(kind);
        let image = cursor.rasterise(100).expect("every built-in renders");
        let any_drawn = image.surface().pixels().iter().any(|pixel| pixel.a > 0);
        assert!(any_drawn, "{kind:?} should draw at least one pixel");
    }
}

/// The four resize kinds, in the order the axes read: the two straight
/// arrows then the two diagonals.
const RESIZE_KINDS: [CursorKind; 4] = [
    CursorKind::ResizeHorizontal,
    CursorKind::ResizeVertical,
    CursorKind::ResizeDiagonalRising,
    CursorKind::ResizeDiagonalFalling,
];

/// The alpha channel of a built-in cursor rasterised at its authored size, as
/// a `side`×`side` grid.
fn builtin_coverage(kind: CursorKind) -> (usize, alloc::vec::Vec<u8>) {
    let image = CursorTheme::builtin()
        .cursor(kind)
        .rasterise(100)
        .expect("renderable");
    let side = usize::try_from(image.width()).unwrap_or(0);
    let alpha = image.surface().pixels().iter().map(|p| p.a).collect();
    (side, alpha)
}

#[test]
fn every_resize_cursor_points_both_ways() {
    // A resize cursor states that an edge can be dragged either way, so its
    // artwork must be unchanged by a half turn about its hotspot. An arrow
    // with one head would pass every other test here and still tell the user
    // the wrong thing.
    for kind in RESIZE_KINDS {
        let (side, alpha) = builtin_coverage(kind);
        assert!(side > 0, "{kind:?} rasterises");
        for y in 0..side {
            for x in 0..side {
                assert_eq!(
                    alpha[y * side + x],
                    alpha[(side - 1 - y) * side + (side - 1 - x)],
                    "{kind:?} is lopsided at ({x}, {y})"
                );
            }
        }
    }
}

#[test]
fn the_resize_cursors_are_one_arrow_at_four_angles() {
    let (side, horizontal) = builtin_coverage(CursorKind::ResizeHorizontal);
    let (_, vertical) = builtin_coverage(CursorKind::ResizeVertical);
    let (_, rising) = builtin_coverage(CursorKind::ResizeDiagonalRising);
    let (_, falling) = builtin_coverage(CursorKind::ResizeDiagonalFalling);
    for y in 0..side {
        for x in 0..side {
            assert_eq!(
                horizontal[y * side + x],
                vertical[x * side + y],
                "the vertical arrow is the horizontal one transposed"
            );
            assert_eq!(
                rising[y * side + x],
                falling[y * side + (side - 1 - x)],
                "the two diagonals are mirror images"
            );
        }
    }
    assert_ne!(
        rising, falling,
        "a window's two corners need opposite diagonals"
    );
}

#[test]
fn every_resize_cursor_pivots_on_its_centre() {
    // The hotspot is the point the edge is dragged from, so it sits at the
    // arrow's middle and scales with the artwork.
    for kind in RESIZE_KINDS {
        let cursor = CursorTheme::builtin().cursor(kind).clone();
        let native = cursor.rasterise(100).expect("renderable");
        let scaled = cursor.rasterise(200).expect("renderable");
        let centre = i32::try_from(native.width() / 2).unwrap_or(0);
        assert_eq!(native.hotspot().x, centre, "{kind:?} x");
        assert_eq!(native.hotspot().y, centre, "{kind:?} y");
        assert_eq!(scaled.hotspot().x, centre * 2, "{kind:?} scaled x");
    }
}

#[test]
fn builtin_arrow_hotspot_is_the_tip() {
    let theme = CursorTheme::builtin();
    let arrow = theme.cursor(CursorKind::Arrow);
    let native = arrow.rasterise(100).expect("renderable");
    assert_eq!(native.hotspot().x, 0);
    assert_eq!(native.hotspot().y, 0);
    // The hotspot scales with the artwork but the arrow tip stays top-left.
    let scaled = arrow.rasterise(200).expect("renderable");
    assert_eq!(scaled.hotspot().x, 0);
    assert_eq!(scaled.hotspot().y, 0);
}

#[test]
fn builtin_centre_hotspot_scales() {
    let theme = CursorTheme::builtin();
    let move_ = theme.cursor(CursorKind::Move);
    let native = move_.rasterise(100).expect("renderable");
    let scaled = move_.rasterise(200).expect("renderable");
    assert!(scaled.hotspot().x > native.hotspot().x);
    assert!(scaled.hotspot().y > native.hotspot().y);
}

#[test]
fn builtin_arrow_layers_a_dark_outline_under_a_light_body() {
    let theme = CursorTheme::builtin();
    let image = theme
        .cursor(CursorKind::Arrow)
        .rasterise(400)
        .expect("renderable");
    let surface = image.surface();
    let mut saw_dark = false;
    let mut saw_light = false;
    for pixel in surface.pixels() {
        if pixel.a < 16 {
            continue;
        }
        let colour = pixel.unpremultiply();
        let luma = u32::from(colour.r) + u32::from(colour.g) + u32::from(colour.b);
        if luma < 200 {
            saw_dark = true;
        }
        if luma > 600 {
            saw_light = true;
        }
    }
    assert!(saw_dark, "the outline layer should contribute dark pixels");
    assert!(saw_light, "the body layer should contribute light pixels");
}

#[test]
fn builtin_busy_cursor_is_two_tone() {
    let theme = CursorTheme::builtin();
    let image = theme
        .cursor(CursorKind::Busy)
        .rasterise(400)
        .expect("renderable");
    let mut saw_blue = false;
    let mut saw_amber = false;
    for pixel in image.surface().pixels() {
        if pixel.a < 200 {
            continue;
        }
        let colour = pixel.unpremultiply();
        if colour.b > colour.r && colour.b > colour.g {
            saw_blue = true;
        }
        if colour.r > colour.b && colour.g > colour.b {
            saw_amber = true;
        }
    }
    assert!(saw_blue, "busy cursor should show its blue tone");
    assert!(saw_amber, "busy cursor should show its amber tone");
}

#[test]
fn registry_holds_builtin_and_is_never_empty() {
    let registry = CursorRegistry::with_builtin();
    assert_eq!(registry.len(), 1);
    assert!(!registry.is_empty());
    assert_eq!(registry.active_id(), CursorSetId::BUILTIN);
    for kind in CURSOR_KINDS {
        // Resolving the active cursor never panics.
        let _ = registry.active_cursor(kind);
    }
}

#[test]
fn registry_set_active_unknown_fails_closed() {
    let mut registry = CursorRegistry::with_builtin();
    let missing = CursorSetId::new("nope");
    assert_eq!(
        registry.set_active(missing),
        Err(CursorRegistryError::UnknownSet(missing))
    );
    assert_eq!(registry.active_id(), CursorSetId::BUILTIN);
}

#[test]
fn registry_register_then_switch_replaces_the_cursor_set() {
    let mut registry = CursorRegistry::with_builtin();
    // A high-visibility set whose arrow is a solid red square.
    let red = Color::rgb(255, 0, 0);
    let custom = CursorTheme::from_cursors(|_| solid_square(16, red));
    let id = CursorSetId::new("high-contrast");
    registry.register(id, custom).expect("fresh id");
    assert_eq!(registry.len(), 2);

    registry.set_active(id).expect("registered");
    assert_eq!(registry.active_id(), id);

    let image = registry
        .active_cursor(CursorKind::Arrow)
        .rasterise(100)
        .expect("renderable");
    let centre = image.surface().get(8, 8).expect("in bounds");
    assert_eq!(centre.unpremultiply(), red);
}

#[test]
fn registry_register_duplicate_id_fails_closed() {
    let mut registry = CursorRegistry::with_builtin();
    let dup = CursorSetId::BUILTIN;
    assert_eq!(
        registry.register(dup, CursorTheme::builtin()),
        Err(CursorRegistryError::DuplicateId(dup))
    );
    assert_eq!(registry.len(), 1);
}

#[test]
fn registry_lists_ids_builtin_first() {
    let mut registry = CursorRegistry::with_builtin();
    let id = CursorSetId::new("extra");
    registry
        .register(id, CursorTheme::builtin())
        .expect("fresh id");
    let ids: alloc::vec::Vec<CursorSetId> = registry.ids().collect();
    assert_eq!(ids, vec![CursorSetId::BUILTIN, id]);
}

#[test]
fn decodes_an_svg_cursor_with_its_hotspot() {
    let svg = br##"<svg viewBox="0 0 24 24" data-hotspot-x="1" data-hotspot-y="2">
        <polygon points="1,1 1,17 5,13 9,21 12,19 8,12 14,12" fill="#000"/>
        <polygon points="2,3 2,14 5,11 8,17 9,16 6,10 11,10" fill="#fff"/>
    </svg>"##;
    let cursor = crate::decode_svg(svg).expect("valid svg cursor");
    // The hotspot is scaled onto the decoder's shared design grid along with
    // the artwork, so it still points at the same place in the drawing.
    assert_eq!(cursor.design_size(), tairix_svg::DESIGN_GRID);
    assert_eq!(cursor.hotspot_x(), HOTSPOT.0);
    assert_eq!(cursor.hotspot_y(), HOTSPOT.1);
    assert_eq!(cursor.shapes().len(), 2);
    assert_eq!(solid(&cursor.shapes()[1]), Color::rgb(255, 255, 255));
}

#[test]
fn decoded_svg_cursor_without_hotspot_pins_to_origin() {
    let svg = br##"<svg viewBox="0 0 16 16"><polygon points="0,0 0,12 4,9 7,15 9,8" fill="#fff"/></svg>"##;
    let cursor = crate::decode_svg(svg).expect("valid svg cursor");
    assert_eq!(cursor.hotspot_x(), 0);
    assert_eq!(cursor.hotspot_y(), 0);
}

#[test]
fn decoded_svg_cursor_rasterises() {
    let svg = br##"<svg viewBox="0 0 16 16"><polygon points="0,0 0,12 4,9 7,15 9,8" fill="#fff"/></svg>"##;
    let cursor = crate::decode_svg(svg).expect("valid svg cursor");
    let image = cursor.rasterise(100).expect("renderable");
    assert!(image.surface().pixels().iter().any(|p| p.a > 0));
}

#[test]
fn malformed_svg_cursor_fails_closed() {
    // The caller substitutes a built-in cursor rather than crashing.
    assert!(crate::decode_svg(b"<svg></svg>").is_err());
}

/// A distinctive SVG cursor (design grid 24, hotspot (1, 2)) so a loaded
/// cursor is told apart from any built-in (design grid 32, origin hotspot).
const LOADED_SVG: &[u8] = br##"<svg viewBox="0 0 24 24" data-hotspot-x="1" data-hotspot-y="2">
    <polygon points="1,1 1,17 5,13 9,21 12,19 8,12 14,12" fill="#fff"/>
</svg>"##;

/// An in-memory cursor-asset source: returns [`LOADED_SVG`] for the listed
/// kinds and the given bytes for everything else (`None` by default).
struct TestSource {
    kinds: &'static [CursorKind],
    other: Option<&'static [u8]>,
}

impl TestSource {
    const fn for_kinds(kinds: &'static [CursorKind]) -> Self {
        Self { kinds, other: None }
    }
}

impl crate::CursorAssetSource for TestSource {
    fn asset(&self, kind: CursorKind) -> Option<&[u8]> {
        if self.kinds.contains(&kind) {
            Some(LOADED_SVG)
        } else {
            self.other
        }
    }
}

fn is_loaded(cursor: &VectorCursor) -> bool {
    cursor.design_size() == tairix_svg::DESIGN_GRID
        && cursor.hotspot_x() == HOTSPOT.0
        && cursor.hotspot_y() == HOTSPOT.1
}

#[test]
fn from_assets_loads_every_kind_when_all_present() {
    let source = TestSource::for_kinds(&CURSOR_KINDS);
    let theme = CursorTheme::from_assets(&source);
    for kind in CURSOR_KINDS {
        assert!(is_loaded(theme.cursor(kind)), "{kind:?} should be loaded");
    }
}

#[test]
fn from_assets_empty_source_yields_builtin_set() {
    let source = TestSource::for_kinds(&[]);
    let theme = CursorTheme::from_assets(&source);
    assert_eq!(theme, CursorTheme::builtin());
    for kind in CURSOR_KINDS {
        assert!(!is_loaded(theme.cursor(kind)), "{kind:?} should fall back");
    }
}

#[test]
fn from_assets_mixes_loaded_and_builtin_fallbacks() {
    let source = TestSource::for_kinds(&[CursorKind::Arrow, CursorKind::Busy]);
    let theme = CursorTheme::from_assets(&source);
    let builtin = CursorTheme::builtin();
    assert!(is_loaded(theme.cursor(CursorKind::Arrow)));
    assert!(is_loaded(theme.cursor(CursorKind::Busy)));
    assert_eq!(
        theme.cursor(CursorKind::Text),
        builtin.cursor(CursorKind::Text)
    );
    assert_eq!(
        theme.cursor(CursorKind::Pointer),
        builtin.cursor(CursorKind::Pointer)
    );
    assert_eq!(
        theme.cursor(CursorKind::Move),
        builtin.cursor(CursorKind::Move)
    );
}

#[test]
fn from_assets_malformed_asset_falls_back_per_kind() {
    // The arrow asset is broken; it must fall back to the built-in arrow
    // while every other kind still loads.
    let source = TestSource {
        kinds: &[
            CursorKind::Text,
            CursorKind::Pointer,
            CursorKind::Move,
            CursorKind::Busy,
        ],
        other: Some(b"<svg></svg>"),
    };
    let theme = CursorTheme::from_assets(&source);
    assert_eq!(
        theme.cursor(CursorKind::Arrow),
        CursorTheme::builtin().cursor(CursorKind::Arrow)
    );
    assert!(is_loaded(theme.cursor(CursorKind::Text)));
}

#[test]
fn from_assets_set_registers_and_activates() {
    let source = TestSource::for_kinds(&CURSOR_KINDS);
    let mut registry = CursorRegistry::with_builtin();
    let id = CursorSetId::new("on-disk");
    registry
        .register(id, CursorTheme::from_assets(&source))
        .expect("fresh id");
    registry.set_active(id).expect("registered");
    assert!(is_loaded(registry.active_cursor(CursorKind::Arrow)));
}
