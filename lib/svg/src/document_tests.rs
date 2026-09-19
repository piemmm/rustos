//! Unit tests for the document walk: what a whole SVG decodes to.
//!
//! The decoder is the desktop's untrusted-asset parser, so these cover both
//! the happy path — each element and structure produces the right layers, in
//! the right order, at the right place — and the fail-closed path, where
//! every malformed or unaffordable document is a precise [`SvgError`] and
//! never a panic.

use core::fmt::Write as _;

use alloc::format;
use alloc::vec::Vec;

use tairix_raster::{
    for_each_fill, Color, FillRule, Group, Layer, MaskKind, Node, Paint, Pattern, Surface, TileFold,
};

use crate::error::SvgError;
use crate::{decode, Viewport, DESIGN_GRID};

/// Fit a document to the square slot, which is what all but the viewport's
/// own tests are about.
fn decode_square(bytes: &[u8]) -> Result<crate::SvgImage, SvgError> {
    decode(bytes, Viewport::Square)
}

/// The nesting bound the decoder and the renderer share.
const DESIGN_NESTING: usize = tairix_raster::MAX_GROUP_DEPTH;

/// The design grid as a contour coordinate.
fn grid() -> i32 {
    i32::try_from(DESIGN_GRID).unwrap_or(0)
}

/// Every test document uses an eight-unit view box, so one user unit is
/// exactly this many design units and every expected coordinate is a round
/// number.
const UNIT: i32 = 256;

/// The layers a document decodes to, flattened out of whatever groups
/// composite them.
#[track_caller]
fn layers(svg: &str) -> Vec<Layer> {
    flatten(
        decode_square(svg.as_bytes())
            .expect("a decodable document")
            .nodes(),
    )
}

/// Every filled layer of an artwork tree, in drawing order.
fn flatten(nodes: &[Node]) -> Vec<Layer> {
    let mut layers = Vec::new();
    for_each_fill(nodes, &mut |layer| layers.push(layer.clone()));
    layers
}

/// A document with `body` inside an eight-unit square view box.
fn document(body: &str) -> alloc::string::String {
    format!(r#"<svg viewBox="0 0 8 8">{body}</svg>"#)
}

/// The solid colour a layer paints with.
#[track_caller]
fn solid(layer: &Layer) -> Color {
    match layer.paint {
        Paint::Solid(color) => color,
        Paint::Gradient(_) | Paint::Pattern(_) => panic!("expected a solid paint"),
    }
}

/// The one contour of a single-contour layer.
#[track_caller]
fn contour(layer: &Layer) -> &[(i32, i32)] {
    assert_eq!(layer.contours.len(), 1, "expected one contour");
    &layer.contours[0]
}

// --- the design grid ------------------------------------------------------

/// Every asset lands on the same grid, whatever its own view box says, so a
/// consumer never has to rescale between assets.
#[test]
fn every_document_lands_on_the_shared_design_grid() {
    for view_box in ["0 0 8 8", "0 0 24 24", "0 0 1000 1000"] {
        let svg = format!(r#"<svg viewBox="{view_box}"><rect width="1" height="1"/></svg>"#);
        let image = decode_square(svg.as_bytes()).expect("a decodable document");
        assert_eq!(image.design(), DESIGN_GRID);
    }
}

#[test]
fn user_coordinates_are_scaled_onto_the_design_grid() {
    let decoded = layers(&document(r#"<rect x="1" y="2" width="4" height="4"/>"#));
    assert_eq!(
        contour(&decoded[0]),
        [
            (UNIT, 2 * UNIT),
            (5 * UNIT, 2 * UNIT),
            (5 * UNIT, 6 * UNIT),
            (UNIT, 6 * UNIT)
        ]
    );
}

/// A view box that does not start at the origin shifts the drawing, which is
/// what lets artwork be authored around any point.
#[test]
fn a_view_box_origin_is_taken_off_the_coordinates() {
    let svg = r#"<svg viewBox="4 4 8 8"><rect x="4" y="4" width="4" height="4"/></svg>"#;
    let decoded = layers(svg);
    assert_eq!(contour(&decoded[0])[0], (0, 0));
}

/// Non-square artwork is letter-boxed into the square slot rather than
/// stretched — the reason the decoder honours `preserveAspectRatio` at all.
#[test]
fn a_non_square_view_box_is_letter_boxed_not_stretched() {
    let svg = r#"<svg viewBox="0 0 16 8"><rect width="16" height="8"/></svg>"#;
    let decoded = layers(svg);
    let points = contour(&decoded[0]);
    let top = points.iter().map(|point| point.1).min().expect("a point");
    let bottom = points.iter().map(|point| point.1).max().expect("a point");
    let left = points.iter().map(|point| point.0).min().expect("a point");
    let right = points.iter().map(|point| point.0).max().expect("a point");
    // Full width, half height, centred in the spare space.
    assert_eq!((left, right), (0, grid()));
    assert_eq!(bottom - top, grid() / 2);
    assert_eq!(top, grid() / 4);
}

#[test]
fn a_document_may_state_its_size_instead_of_a_view_box() {
    let svg = r#"<svg width="8" height="8"><rect width="8" height="8"/></svg>"#;
    let decoded = layers(svg);
    assert_eq!(contour(&decoded[0])[2], (8 * UNIT, 8 * UNIT));
}

// --- painting order and layers --------------------------------------------

#[test]
fn layers_keep_document_order_for_bottom_first_stacking() {
    let decoded = layers(&document(
        r#"<rect width="8" height="8" fill="black"/>
           <rect width="4" height="4" fill="white"/>"#,
    ));
    assert_eq!(decoded.len(), 2);
    assert_eq!(solid(&decoded[0]), Color::rgb(0, 0, 0));
    assert_eq!(solid(&decoded[1]), Color::rgb(255, 255, 255));
}

/// SVG paints a shape's fill and then its stroke, so a stroked shape is two
/// layers in that order.
#[test]
fn a_stroked_shape_paints_its_fill_then_its_outline() {
    let decoded = layers(&document(
        r#"<rect x="2" y="2" width="4" height="4" fill="red" stroke="blue" stroke-width="1"/>"#,
    ));
    assert_eq!(decoded.len(), 2);
    assert_eq!(solid(&decoded[0]), Color::rgb(255, 0, 0));
    assert_eq!(solid(&decoded[1]), Color::rgb(0, 0, 255));
    assert_eq!(decoded[1].rule, FillRule::NonZero);
    assert!(
        decoded[1].contours.len() > 1,
        "a stroke outline is many pieces unioned together"
    );
}

#[test]
fn the_default_fill_is_black_and_an_explicit_none_paints_nothing() {
    assert_eq!(
        solid(&layers(&document(r#"<rect width="8" height="8"/>"#))[0]),
        Color::rgb(0, 0, 0)
    );
    assert!(layers(&document(r#"<rect width="8" height="8" fill="none"/>"#)).is_empty());
    assert!(layers(&document(
        r#"<rect width="8" height="8" fill-opacity="0"/>"#
    ))
    .is_empty());
}

#[test]
fn the_fill_rule_reaches_the_layer() {
    let decoded = layers(&document(
        r#"<path d="M0 0 H8 V8 H0 Z M2 2 H6 V6 H2 Z" fill-rule="evenodd"/>"#,
    ));
    assert_eq!(decoded[0].rule, FillRule::EvenOdd);
    assert_eq!(decoded[0].contours.len(), 2, "a shape with a hole");
}

#[test]
fn a_shape_that_encloses_no_area_contributes_no_layer() {
    assert!(layers(&document(r#"<polygon points="0,0 8,8"/>"#)).is_empty());
    assert!(layers(&document(r#"<line x1="0" y1="0" x2="8" y2="8"/>"#)).is_empty());
}

// --- structure ------------------------------------------------------------

#[test]
fn a_group_hands_its_style_and_transform_to_its_children() {
    let decoded = layers(&document(
        r##"<g fill="#00ff00" transform="translate(1 1)">
              <rect width="2" height="2"/>
            </g>"##,
    ));
    assert_eq!(solid(&decoded[0]), Color::rgb(0, 255, 0));
    assert_eq!(contour(&decoded[0])[0], (UNIT, UNIT));
}

#[test]
fn nested_transforms_compose_outward() {
    let decoded = layers(&document(
        r#"<g transform="translate(2 0)">
             <g transform="scale(2)"><rect width="1" height="1"/></g>
           </g>"#,
    ));
    // Scaled first, then translated: the far corner lands at user (4, 2).
    assert_eq!(contour(&decoded[0])[2], (4 * UNIT, 2 * UNIT));
}

/// A definition is drawn only where it is referenced; descending into it
/// would paint its contents twice.
#[test]
fn definitions_are_not_drawn_where_they_are_written() {
    let decoded = layers(&document(
        r#"<defs><rect width="8" height="8"/></defs>
           <rect width="1" height="1"/>"#,
    ));
    assert_eq!(decoded.len(), 1);
    assert_eq!(contour(&decoded[0])[2], (UNIT, UNIT));
}

#[test]
fn a_use_draws_what_it_references_where_it_asks() {
    let decoded = layers(&document(
        r##"<defs><rect id="box" width="2" height="2"/></defs>
           <use href="#box" x="4" y="4"/>"##,
    ));
    assert_eq!(decoded.len(), 1);
    assert_eq!(contour(&decoded[0])[0], (4 * UNIT, 4 * UNIT));
}

#[test]
fn a_use_may_reference_a_symbol_and_may_use_the_older_link_spelling() {
    let decoded = layers(&document(
        r##"<defs><symbol id="s"><rect width="2" height="2"/></symbol></defs>
           <use xlink:href="#s" x="2" y="2"/>"##,
    ));
    assert_eq!(decoded.len(), 1);
    assert_eq!(contour(&decoded[0])[0], (2 * UNIT, 2 * UNIT));
}

#[test]
fn a_use_that_references_nothing_draws_nothing() {
    assert!(layers(&document(r##"<use href="#absent"/>"##)).is_empty());
}

/// A reference cycle must be refused rather than followed for ever.
#[test]
fn a_use_cycle_is_refused() {
    let svg = document(
        r##"<g id="a"><use href="#b"/></g>
           <g id="b"><use href="#a"/></g>"##,
    );
    assert_eq!(decode_square(svg.as_bytes()), Err(SvgError::TooComplex));
}

/// A `switch` renders the first child it can, and only that one.
#[test]
fn a_switch_draws_only_its_first_usable_child() {
    let decoded = layers(&document(
        r#"<switch>
             <rect requiredExtensions="http://example.invalid" width="8" height="8"/>
             <rect width="2" height="2"/>
             <rect width="4" height="4"/>
           </switch>"#,
    ));
    assert_eq!(decoded.len(), 1);
    assert_eq!(contour(&decoded[0])[2], (2 * UNIT, 2 * UNIT));
}

#[test]
fn a_nested_svg_establishes_its_own_viewport() {
    let decoded = layers(&document(
        r#"<svg x="4" y="4" width="4" height="4" viewBox="0 0 2 2">
             <rect width="2" height="2"/>
           </svg>"#,
    ));
    // The inner drawing fills the inner viewport, which sits in the bottom
    // right quarter of the outer one.
    assert_eq!(contour(&decoded[0])[0], (4 * UNIT, 4 * UNIT));
    assert_eq!(contour(&decoded[0])[2], (8 * UNIT, 8 * UNIT));
}

#[test]
fn display_none_hides_a_whole_subtree_and_visibility_hides_only_the_element() {
    assert!(layers(&document(
        r#"<g display="none"><rect width="8" height="8"/></g>"#
    ))
    .is_empty());

    let decoded = layers(&document(
        r#"<g visibility="hidden"><rect width="8" height="8" visibility="visible"/></g>"#,
    ));
    assert_eq!(decoded.len(), 1);
}

// --- curves, arcs, and strokes end to end ---------------------------------

#[test]
fn a_curved_path_becomes_a_flattened_contour() {
    let decoded = layers(&document(r#"<path d="M0 4 C0 0 8 0 8 4 Z"/>"#));
    assert!(
        contour(&decoded[0]).len() > 8,
        "a curve should flatten to many vertices"
    );
}

#[test]
fn a_circle_becomes_a_closed_ring_on_the_grid() {
    let decoded = layers(&document(r#"<circle cx="4" cy="4" r="2"/>"#));
    let points = contour(&decoded[0]);
    assert!(points.len() > 16);
    for point in points {
        let (dx, dy) = (f64::from(point.0 - 4 * UNIT), f64::from(point.1 - 4 * UNIT));
        let radius = tairix_util::mathf::sqrt(dx * dx + dy * dy);
        assert!((radius - f64::from(2 * UNIT)).abs() <= 2.0);
    }
}

#[test]
fn a_stroke_with_no_width_or_no_paint_draws_nothing() {
    assert!(layers(&document(
        r#"<line x1="0" y1="0" x2="8" y2="8" stroke="black" stroke-width="0"/>"#
    ))
    .is_empty());
    assert!(layers(&document(
        r#"<line x1="0" y1="0" x2="8" y2="8" stroke="none" stroke-width="2"/>"#
    ))
    .is_empty());
}

// --- the hotspot ----------------------------------------------------------

#[test]
fn a_hotspot_is_read_and_scaled_onto_the_grid() {
    let svg = r#"<svg viewBox="0 0 8 8" data-hotspot-x="2" data-hotspot-y="3"><rect width="1" height="1"/></svg>"#;
    let image = decode_square(svg.as_bytes()).expect("a decodable document");
    assert_eq!(image.hotspot(), Some((2 * UNIT, 3 * UNIT)));
}

#[test]
fn no_hotspot_is_none_and_half_a_hotspot_is_refused() {
    let svg = document(r#"<rect width="1" height="1"/>"#);
    assert_eq!(
        decode_square(svg.as_bytes())
            .expect("a decodable document")
            .hotspot(),
        None
    );
    let half = r#"<svg viewBox="0 0 8 8" data-hotspot-x="2"><rect width="1" height="1"/></svg>"#;
    assert_eq!(decode_square(half.as_bytes()), Err(SvgError::InvalidNumber));
}

// --- the XML layer --------------------------------------------------------

#[test]
fn comments_instructions_doctypes_and_character_data_are_skipped() {
    let svg = r#"<?xml version="1.0"?>
        <!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "svg11.dtd">
        <svg viewBox="0 0 8 8">
          <!-- a comment with <angle> brackets -->
          <title>An icon &amp; its name</title>
          <desc><![CDATA[ raw <text> here ]]></desc>
          <rect width="8" height="8"/>
        </svg>"#;
    assert_eq!(layers(svg).len(), 1);
}

/// An element in another namespace is not SVG and is not drawn, however
/// familiar its local name looks.
#[test]
fn a_foreign_namespace_element_is_not_drawn() {
    let svg = r#"<svg viewBox="0 0 8 8" xmlns:sodipodi="http://example.invalid/ns">
          <sodipodi:rect width="8" height="8"/>
          <rect width="1" height="1"/>
        </svg>"#;
    assert_eq!(layers(svg).len(), 1);
}

#[test]
fn an_svg_prefixed_element_is_drawn() {
    let svg = r#"<s:svg xmlns:s="http://www.w3.org/2000/svg" viewBox="0 0 8 8">
          <s:rect width="8" height="8"/>
        </s:svg>"#;
    assert_eq!(layers(svg).len(), 1);
}

// --- refusals -------------------------------------------------------------

#[test]
fn a_document_that_is_not_svg_is_refused() {
    assert_eq!(
        decode_square(b"<html><body/></html>"),
        Err(SvgError::MissingRoot)
    );
    assert_eq!(decode_square(b""), Err(SvgError::MissingRoot));
    assert_eq!(decode_square(&[0xff, 0xfe, 0xfd]), Err(SvgError::NotUtf8));
}

#[test]
fn a_document_with_no_coordinate_system_is_refused() {
    assert_eq!(
        decode_square(br#"<svg><rect width="1" height="1"/></svg>"#),
        Err(SvgError::MissingViewBox)
    );
    assert_eq!(
        decode_square(br#"<svg viewBox="0 0 0 8"><rect width="1" height="1"/></svg>"#),
        Err(SvgError::InvalidViewBox)
    );
}

#[test]
fn malformed_xml_is_refused() {
    for bad in [
        r#"<svg viewBox="0 0 8 8"><rect"#,
        r#"<svg viewBox="0 0 8 8"><rect width="1></svg>"#,
        r#"<svg viewBox="0 0 8 8"><g></svg>"#,
        r#"<svg viewBox="0 0 8 8"></g></svg>"#,
        r#"<svg viewBox="0 0 8 8"><!-- unterminated"#,
    ] {
        assert_eq!(
            decode_square(bad.as_bytes()),
            Err(SvgError::Malformed),
            "{bad:?} should be refused"
        );
    }
}

#[test]
fn a_malformed_value_anywhere_refuses_the_whole_document() {
    assert_eq!(
        decode_square(document(r#"<rect width="1" height="1" fill="chartreuseish"/>"#).as_bytes()),
        Err(SvgError::InvalidColor)
    );
    assert_eq!(
        decode_square(document(r#"<rect width="1" height="1" transform="wobble(2)"/>"#).as_bytes()),
        Err(SvgError::InvalidNumber)
    );
    assert_eq!(
        decode_square(document(r#"<path d="M0 0 X1 1"/>"#).as_bytes()),
        Err(SvgError::UnsupportedPath)
    );
}

/// The bounds are what stop a hostile asset from exhausting memory or draw
/// time before it has shown anything.
#[test]
fn an_unaffordable_document_is_refused() {
    let opened = "<g>".repeat(200);
    let closed = "</g>".repeat(200);
    let deep = format!(r#"{opened}<rect width="1" height="1"/>{closed}"#);
    assert_eq!(
        decode_square(document(&deep).as_bytes()),
        Err(SvgError::TooComplex)
    );

    let many = "<rect width=\"1\" height=\"1\"/>".repeat(9000);
    assert_eq!(
        decode_square(document(&many).as_bytes()),
        Err(SvgError::TooComplex)
    );
}

/// Whatever the input, the decoder answers: it never panics, and never emits
/// a layer with no contours.
#[test]
fn assorted_hostile_documents_never_panic() {
    let cases = [
        "<svg",
        "<svg/>",
        "<svg viewBox/>",
        r#"<svg viewBox="0 0 8 8"/>"#,
        r#"<svg viewBox="0 0 8 8"><rect width="1e400" height="1"/></svg>"#,
        r##"<svg viewBox="0 0 8 8"><use href="#self" id="self"/></svg>"##,
        r#"<svg viewBox="0 0 8 8"><path d="M0 0A0 0 0 0 0 0 0"/></svg>"#,
        r#"<svg viewBox="0 0 8 8"><g transform="matrix(0 0 0 0 0 0)"><rect width="8" height="8" fill="url(#g)"/></g></svg>"#,
        r#"<svg viewBox="0 0 8 8"><rect width="8" height="8" stroke="black" stroke-width="1e300"/></svg>"#,
        r#"<svg viewBox="0 0 8 8" preserveAspectRatio="none slice"><rect width="8" height="8"/></svg>"#,
        r#"<svg viewBox="-1e300 -1e300 1e300 1e300"><rect width="8" height="8"/></svg>"#,
    ];
    for case in cases {
        if let Ok(image) = decode_square(case.as_bytes()) {
            for layer in flatten(image.nodes()) {
                assert!(!layer.contours.is_empty(), "{case:?} made an empty layer");
            }
        }
    }
}

// --- the viewport a document is fitted to ---------------------------------

/// The contour of the only layer a document decodes to, under `viewport`.
#[track_caller]
fn only_contour(svg: &str, viewport: Viewport) -> Vec<(i32, i32)> {
    let image = decode(svg.as_bytes(), viewport).expect("a decodable document");
    let decoded = flatten(image.nodes());
    assert_eq!(decoded.len(), 1, "expected one layer");
    contour(&decoded[0]).to_vec()
}

/// A wide document holding a rectangle over the whole of its own view box.
fn wide() -> alloc::string::String {
    r#"<svg viewBox="0 0 16 4"><rect width="16" height="4"/></svg>"#.into()
}

#[test]
fn the_square_slot_letter_boxes_a_drawing_that_is_not_square() {
    // Sixteen by four into a square grid: the drawing keeps its shape, so
    // it occupies a quarter of the height and is centred in the spare
    // space.
    let got = only_contour(&wide(), Viewport::Square);
    let quarter = grid() / 4;
    let top = (grid() - quarter) / 2;
    assert!(got.contains(&(0, top)), "{got:?}");
    assert!(got.contains(&(grid(), top + quarter)), "{got:?}");
}

#[test]
fn the_natural_shape_fills_the_grid_on_both_axes() {
    // The same drawing normalised: no bands, and full precision on the
    // short axis rather than a quarter of it.
    let got = only_contour(&wide(), Viewport::Natural);
    assert!(got.contains(&(0, 0)), "{got:?}");
    assert!(got.contains(&(grid(), grid())), "{got:?}");
}

#[test]
fn the_authored_shape_is_carried_whichever_viewport_is_asked_for() {
    // What a consumer rasterising the natural form sizes its surface from.
    for viewport in [Viewport::Square, Viewport::Natural] {
        let image = decode(wide().as_bytes(), viewport).expect("a decodable document");
        assert_eq!(image.source_extent(), (16.0, 4.0), "{viewport:?}");
        assert_eq!(image.design(), DESIGN_GRID, "{viewport:?}");
    }
}

#[test]
fn a_square_document_decodes_the_same_under_either_viewport() {
    // Fitting a shape to its own shape is the same map however it is
    // spelled, so the two viewports can only differ for a drawing that is
    // not square.
    let svg = document(r#"<rect x="1" y="2" width="4" height="3"/>"#);
    assert_eq!(
        decode(svg.as_bytes(), Viewport::Square),
        decode(svg.as_bytes(), Viewport::Natural)
    );
}

#[test]
fn the_natural_shape_does_not_read_preserve_aspect_ratio() {
    // Every anchoring names a different placement in a square slot, and
    // none of them means anything once the viewport is the drawing's own
    // shape.
    let mut placements = Vec::new();
    for ratio in ["xMinYMin", "xMidYMid", "xMaxYMax", "none"] {
        let svg = format!(
            r#"<svg viewBox="0 0 16 4" preserveAspectRatio="{ratio}"><rect width="16" height="4"/></svg>"#
        );
        let natural = only_contour(&svg, Viewport::Natural);
        assert_eq!(
            natural,
            only_contour(&wide(), Viewport::Natural),
            "{ratio} moved the natural fit"
        );
        placements.push(only_contour(&svg, Viewport::Square));
    }
    // The square slot, by contrast, places each one somewhere different.
    placements.dedup();
    assert_eq!(placements.len(), 4, "the square slot ignored an anchoring");
}

#[test]
fn a_malformed_preserve_aspect_ratio_refuses_the_document_under_both() {
    // The attribute means nothing to the natural fit, but a document is
    // well formed or it is not — that cannot depend on who is asking.
    let svg = r#"<svg viewBox="0 0 8 8" preserveAspectRatio="sideways"><rect width="1" height="1"/></svg>"#;
    for viewport in [Viewport::Square, Viewport::Natural] {
        assert_eq!(
            decode(svg.as_bytes(), viewport),
            Err(SvgError::InvalidViewBox),
            "{viewport:?}"
        );
    }
}

#[test]
fn a_stroke_is_carried_into_the_stretch_rather_than_dropped_from_it() {
    // A round pen over a stretched drawing is an ellipse on the grid, and
    // becomes a round pen again in a surface of the drawing's own shape.
    // What matters here is that the stroke still produces its own layer and
    // spans the wider axis further than the narrow one.
    let svg = r#"<svg viewBox="0 0 16 4"><line x1="0" y1="2" x2="16" y2="2" stroke="black" stroke-width="2"/></svg>"#;
    let image = decode(svg.as_bytes(), Viewport::Natural).expect("a decodable document");
    let decoded = flatten(image.nodes());
    assert_eq!(decoded.len(), 1, "the stroke is the only layer");
    let points = contour(&decoded[0]);
    let height = points.iter().map(|&(_, y)| y).max().unwrap_or(0)
        - points.iter().map(|&(_, y)| y).min().unwrap_or(0);
    // Two user units of a four-unit box is half the grid once stretched.
    assert_eq!(height, grid() / 2);
}

// --- compositing: group opacity, clipping, masking ------------------------

/// The artwork a document decodes to, top level only.
#[track_caller]
fn tree(svg: &str) -> Vec<Node> {
    decode_square(svg.as_bytes())
        .expect("a decodable document")
        .nodes()
        .to_vec()
}

/// The one group a document decodes to.
#[track_caller]
fn only_group(svg: &str) -> Group {
    match tree(svg).as_slice() {
        [Node::Group(group)] => group.clone(),
        other => panic!("expected one group, got {} node(s)", other.len()),
    }
}

/// The axis-aligned box a layer's contours fall inside.
#[track_caller]
fn box_of(layer: &Layer) -> (i32, i32, i32, i32) {
    layer
        .contours
        .iter()
        .flatten()
        .fold((i32::MAX, i32::MAX, i32::MIN, i32::MIN), |acc, point| {
            (
                acc.0.min(point.0),
                acc.1.min(point.1),
                acc.2.max(point.0),
                acc.3.max(point.1),
            )
        })
}

/// A group opacity composites the subtree as a unit, because weakening each
/// shape first and compositing after is a different picture.
#[test]
fn a_container_opacity_becomes_a_group() {
    let group = only_group(&document(
        r#"<g opacity="0.5"><rect width="8" height="8" fill="red"/></g>"#,
    ));
    assert_eq!(group.opacity, 128);
    assert!(group.mask.is_none());
    assert_eq!(group.children.len(), 1);
    // The child keeps its own colour: the opacity is the group's.
    assert_eq!(solid(&flatten(&group.children)[0]).a, 255);
}

/// One layer composited at a group opacity is the same pixels as that layer
/// painted at the product, so the common translucent shape costs no buffer.
#[test]
fn a_lone_layer_folds_its_elements_opacity_instead_of_grouping() {
    let decoded = tree(&document(
        r#"<rect width="8" height="8" fill="red" opacity="0.5"/>"#,
    ));
    let [Node::Fill(layer)] = decoded.as_slice() else {
        panic!("expected one plain layer");
    };
    assert_eq!(solid(layer).a, 128);
}

/// A fill and its own stroke overlap, so folding the opacity into each would
/// show the fill through the stroke.
#[test]
fn a_fill_and_its_stroke_are_composited_as_a_unit() {
    let group = only_group(&document(
        r#"<rect width="6" height="6" fill="red" stroke="blue" stroke-width="2" opacity="0.5"/>"#,
    ));
    assert_eq!(group.opacity, 128);
    let inner = flatten(&group.children);
    assert_eq!(inner.len(), 2);
    assert_eq!(solid(&inner[0]).a, 255);
    assert_eq!(solid(&inner[1]).a, 255);
}

#[test]
fn a_fully_transparent_element_draws_nothing() {
    assert!(tree(&document(r#"<rect width="8" height="8" opacity="0"/>"#)).is_empty());
}

#[test]
fn a_clip_path_becomes_an_alpha_mask_of_its_shapes() {
    let group = only_group(&document(
        r#"<clipPath id="c"><rect width="4" height="8"/></clipPath>
           <rect width="8" height="8" fill="red" clip-path="url(#c)"/>"#,
    ));
    assert_eq!(group.opacity, 255);
    let mask = group.mask.expect("a clip mask");
    assert_eq!(mask.kind, MaskKind::Alpha);
    let shapes = flatten(&mask.content);
    assert_eq!(shapes.len(), 1);
    // The clip covers the left half of the eight-unit box, opaquely.
    assert_eq!(box_of(&shapes[0]), (0, 0, 4 * UNIT, 8 * UNIT));
    assert_eq!(solid(&shapes[0]).a, 255);
    assert_eq!(flatten(&group.children).len(), 1);
}

/// Several shapes in one clip union, because opaque over opaque is opaque —
/// which is why a clip needs no second rule for it.
#[test]
fn a_clip_path_with_several_shapes_keeps_them_all() {
    let group = only_group(&document(
        r#"<clipPath id="c"><rect width="4" height="8"/><circle cx="6" cy="6" r="2"/></clipPath>
           <rect width="8" height="8" fill="red" clip-path="url(#c)"/>"#,
    ));
    assert_eq!(flatten(&group.mask.expect("a clip mask").content).len(), 2);
}

/// `objectBoundingBox` units are fractions of the clipped element's own box,
/// which only drawing that element can say.
#[test]
fn a_bounding_box_clip_is_resolved_against_the_element_it_clips() {
    let group = only_group(&document(
        r#"<clipPath id="c" clipPathUnits="objectBoundingBox"><rect width="0.5" height="1"/></clipPath>
           <rect x="2" y="2" width="4" height="4" fill="red" clip-path="url(#c)"/>"#,
    ));
    let shapes = flatten(&group.mask.expect("a clip mask").content);
    // Half of a box running 2..6 on both axes is 2..4 across and 2..6 down.
    assert_eq!(box_of(&shapes[0]), (2 * UNIT, 2 * UNIT, 4 * UNIT, 6 * UNIT));
}

/// A container's bounding box is its descendants' geometry in its own space,
/// accumulated as the subtree is drawn.
#[test]
fn a_bounding_box_clip_on_a_container_uses_the_subtrees_box() {
    let group = only_group(&document(
        r#"<clipPath id="c" clipPathUnits="objectBoundingBox"><rect width="1" height="0.5"/></clipPath>
           <g clip-path="url(#c)"><rect x="1" y="1" width="2" height="2" fill="red"/>
           <rect x="3" y="3" width="2" height="2" fill="blue"/></g>"#,
    ));
    let shapes = flatten(&group.mask.expect("a clip mask").content);
    // The union runs 1..5 on both axes; the top half of that is 1..3 down.
    assert_eq!(box_of(&shapes[0]), (UNIT, UNIT, 5 * UNIT, 3 * UNIT));
}

/// The object bounding box is the geometry's, whatever it is drawn with, so
/// a stroke must not widen it.
#[test]
fn a_bounding_box_ignores_the_stroke_width() {
    let group = only_group(&document(
        r#"<clipPath id="c" clipPathUnits="objectBoundingBox"><rect width="1" height="1"/></clipPath>
           <g clip-path="url(#c)"><rect x="2" y="2" width="4" height="4" fill="red"
              stroke="blue" stroke-width="2"/></g>"#,
    ));
    let shapes = flatten(&group.mask.expect("a clip mask").content);
    assert_eq!(box_of(&shapes[0]), (2 * UNIT, 2 * UNIT, 6 * UNIT, 6 * UNIT));
}

/// A clip on a `<clipPath>` intersects the two, which nests rather than
/// needing a rule of its own.
#[test]
fn a_clip_path_may_itself_be_clipped() {
    let group = only_group(&document(
        r#"<clipPath id="outer"><rect width="8" height="4"/></clipPath>
           <clipPath id="inner" clip-path="url(#outer)"><rect width="4" height="8"/></clipPath>
           <rect width="8" height="8" fill="red" clip-path="url(#inner)"/>"#,
    ));
    let content = group.mask.expect("a clip mask").content;
    let [Node::Group(nested)] = content.as_slice() else {
        panic!("expected the inner clip to be clipped in turn");
    };
    assert_eq!(
        nested.mask.as_ref().expect("the outer clip").kind,
        MaskKind::Alpha
    );
}

/// Drawing an element unclipped because the clip could not be found would be
/// a wrong picture where an empty one is an honest refusal.
#[test]
fn an_unresolvable_clip_or_mask_reference_draws_nothing() {
    for reference in ["clip-path", "mask"] {
        let svg = document(&format!(
            r#"<rect width="8" height="8" fill="red" {reference}="url(#nope)"/>"#
        ));
        assert!(tree(&svg).is_empty(), "{reference} drew unmasked artwork");
    }
    // A reference that names an element of the wrong kind is no better.
    let crossed = document(
        r#"<mask id="m"><rect width="8" height="8" fill="white"/></mask>
            <rect width="8" height="8" fill="red" clip-path="url(#m)"/>"#,
    );
    assert!(tree(&crossed).is_empty());
}

#[test]
fn a_mask_reads_its_contents_luminance_by_default() {
    let group = only_group(&document(
        r#"<mask id="m"><rect width="4" height="8" fill="white"/></mask>
           <rect width="8" height="8" fill="red" mask="url(#m)"/>"#,
    ));
    let mask = group.mask.expect("a mask");
    assert_eq!(mask.kind, MaskKind::Luminance);
    assert_eq!(flatten(&mask.content).len(), 1);
}

#[test]
fn a_mask_may_ask_for_its_alpha_instead() {
    for spelling in [r#"mask-type="alpha""#, r#"style="mask-type:alpha""#] {
        let svg = document(&format!(
            r#"<mask id="m" {spelling}><rect width="4" height="8" fill="white"/></mask>
               <rect width="8" height="8" fill="red" mask="url(#m)"/>"#
        ));
        assert_eq!(
            only_group(&svg).mask.expect("a mask").kind,
            MaskKind::Alpha,
            "{spelling}"
        );
    }
}

/// The region bounds the mask itself; content outside it is cut away rather
/// than let through.
#[test]
fn a_mask_region_narrower_than_its_content_clips_it() {
    let group = only_group(&document(
        r#"<mask id="m" maskUnits="userSpaceOnUse" x="0" y="0" width="4" height="8">
             <rect width="8" height="8" fill="white"/>
           </mask>
           <rect width="8" height="8" fill="red" mask="url(#m)"/>"#,
    ));
    let content = group.mask.expect("a mask").content;
    let [Node::Group(region)] = content.as_slice() else {
        panic!("expected the content to be confined to the region");
    };
    let bounds = flatten(&region.mask.as_ref().expect("the region").content);
    assert_eq!(box_of(&bounds[0]), (0, 0, 4 * UNIT, 8 * UNIT));
}

/// The default region is a tenth past the object box on every side, which no
/// content authored inside that box reaches — so it costs no buffer.
#[test]
fn a_mask_region_wider_than_its_content_adds_no_group() {
    let group = only_group(&document(
        r#"<mask id="m"><rect width="8" height="8" fill="white"/></mask>
           <rect width="8" height="8" fill="red" mask="url(#m)"/>"#,
    ));
    let content = group.mask.expect("a mask").content;
    assert!(matches!(content.as_slice(), [Node::Fill(_)]));
}

#[test]
fn mask_content_units_place_the_content_in_the_elements_box() {
    let group = only_group(&document(
        r#"<mask id="m" maskContentUnits="objectBoundingBox">
             <rect width="0.5" height="1" fill="white"/>
           </mask>
           <rect x="2" y="2" width="4" height="4" fill="red" mask="url(#m)"/>"#,
    ));
    let shapes = flatten(&group.mask.expect("a mask").content);
    assert_eq!(box_of(&shapes[0]), (2 * UNIT, 2 * UNIT, 4 * UNIT, 6 * UNIT));
}

/// Nesting past the renderer's bound is refused here rather than decoded into
/// a tree the renderer would then have to turn away.
#[test]
fn groups_nested_past_the_bound_refuse_the_document() {
    let mut body = alloc::string::String::from(r#"<rect width="8" height="8" fill="red"/>"#);
    for _ in 0..=tairix_raster::MAX_GROUP_DEPTH {
        body = format!(r#"<g opacity="0.5">{body}</g>"#);
    }
    assert_eq!(
        decode_square(document(&body).as_bytes()),
        Err(SvgError::TooComplex)
    );
}

/// A clip's own shapes are artwork the renderer must fill, so they are
/// charged against the same budgets everything else is.
#[test]
fn a_clips_geometry_is_charged_against_the_layer_budget() {
    let mut body = alloc::string::String::new();
    for index in 0..1100 {
        let _ = write!(
            body,
            r#"<clipPath id="c{index}"><rect width="8" height="8"/></clipPath>
               <rect width="8" height="8" fill="red" clip-path="url(#c{index})"/>"#
        );
    }
    assert_eq!(
        decode_square(document(&body).as_bytes()),
        Err(SvgError::TooComplex)
    );
}

// --- viewports a `<use>` establishes --------------------------------------

/// A `<symbol>`'s own `viewBox` is fitted to the extent the `<use>` states,
/// which is what makes one symbol serve every size it is drawn at.
#[test]
fn a_symbol_is_fitted_to_the_extent_its_use_states() {
    let svg = r##"<svg viewBox="0 0 8 8">
        <symbol id="s" viewBox="0 0 2 2"><rect width="2" height="2" fill="red"/></symbol>
        <use href="#s" width="8" height="8"/></svg>"##;
    let decoded = layers(svg);
    assert_eq!(decoded.len(), 1);
    assert_eq!(box_of(&decoded[0]), (0, 0, 8 * UNIT, 8 * UNIT));
}

#[test]
fn a_symbol_is_confined_to_the_slot_it_was_given() {
    let svg = r##"<svg viewBox="0 0 8 8">
        <symbol id="s"><rect width="8" height="8" fill="red"/></symbol>
        <use href="#s" width="4" height="8"/></svg>"##;
    let group = only_group(svg);
    let bounds = flatten(&group.mask.expect("the viewport clip").content);
    assert_eq!(box_of(&bounds[0]), (0, 0, 4 * UNIT, 8 * UNIT));
}

#[test]
fn a_nested_viewport_may_let_its_content_spill() {
    let svg = r##"<svg viewBox="0 0 8 8">
        <symbol id="s" overflow="visible"><rect width="8" height="8" fill="red"/></symbol>
        <use href="#s" width="4" height="8"/></svg>"##;
    assert!(matches!(tree(svg).as_slice(), [Node::Fill(_)]));
}

#[test]
fn a_nested_svg_clips_to_its_own_viewport() {
    let svg = r#"<svg viewBox="0 0 8 8">
        <svg width="4" height="8"><rect width="8" height="8" fill="red"/></svg></svg>"#;
    let group = only_group(svg);
    let bounds = flatten(&group.mask.expect("the viewport clip").content);
    assert_eq!(box_of(&bounds[0]), (0, 0, 4 * UNIT, 8 * UNIT));
}

// --- conditional processing ------------------------------------------------

/// A condition this decoder cannot show is satisfied is not met, which is
/// what reaches the unconditional fallback an author writes beside it.
#[test]
fn a_switch_passes_over_a_condition_it_cannot_meet() {
    for condition in [
        r#"systemLanguage="zz""#,
        r#"requiredExtensions="http://example.invalid""#,
        r#"requiredFeatures="http://example.invalid""#,
    ] {
        let svg = document(&format!(
            r#"<switch><rect {condition} width="4" height="4" fill="red"/>
               <rect width="8" height="8" fill="blue"/></switch>"#
        ));
        let decoded = layers(&svg);
        assert_eq!(decoded.len(), 1, "{condition}");
        assert_eq!(solid(&decoded[0]), Color::rgb(0, 0, 255), "{condition}");
    }
}

/// An empty condition requires nothing, so it is met.
#[test]
fn a_switch_takes_a_candidate_whose_condition_is_empty() {
    let svg = document(
        r#"<switch><rect requiredFeatures="" width="4" height="4" fill="red"/>
           <rect width="8" height="8" fill="blue"/></switch>"#,
    );
    assert_eq!(solid(&layers(&svg)[0]), Color::rgb(255, 0, 0));
}

/// Only a graphics or container element is a candidate; a `<desc>` first
/// child would otherwise swallow the whole switch.
#[test]
fn a_switch_skips_a_child_that_draws_nothing_by_kind() {
    let svg = document(
        r#"<switch><desc>what this is</desc><rect width="8" height="8" fill="blue"/></switch>"#,
    );
    let decoded = layers(&svg);
    assert_eq!(decoded.len(), 1);
    assert_eq!(solid(&decoded[0]), Color::rgb(0, 0, 255));
}

// --- paint order -----------------------------------------------------------

#[test]
fn paint_order_may_put_the_stroke_under_the_fill() {
    let body = r#"<rect width="8" height="8" fill="red" stroke="blue" stroke-width="2""#;
    let normal = layers(&document(&format!("{body}/>")));
    assert_eq!(solid(&normal[0]), Color::rgb(255, 0, 0));
    assert_eq!(solid(&normal[1]), Color::rgb(0, 0, 255));

    for order in ["stroke", "stroke fill", "markers stroke fill"] {
        let reordered = layers(&document(&format!(r#"{body} paint-order="{order}"/>"#)));
        assert_eq!(solid(&reordered[0]), Color::rgb(0, 0, 255), "{order}");
        assert_eq!(solid(&reordered[1]), Color::rgb(255, 0, 0), "{order}");
    }
}

// --- the stylesheet in the cascade ----------------------------------------

/// Presentation attribute, then stylesheet, then the `style` attribute, then
/// whatever is `!important` — the order the cascade defines.
#[test]
fn the_stylesheet_sits_between_the_attribute_and_the_style_declaration() {
    let attribute_only = document(
        r##"<style>rect{fill:#00ff00}</style><rect width="8" height="8" fill="#ff0000"/>"##,
    );
    assert_eq!(solid(&layers(&attribute_only)[0]), Color::rgb(0, 255, 0));

    let with_style = document(
        r##"<style>rect{fill:#00ff00}</style>
            <rect width="8" height="8" fill="#ff0000" style="fill:#0000ff"/>"##,
    );
    assert_eq!(solid(&layers(&with_style)[0]), Color::rgb(0, 0, 255));

    let important = document(
        r#"<style>rect{fill:#00ff00 !important}</style>
            <rect width="8" height="8" style="fill:#0000ff"/>"#,
    );
    assert_eq!(solid(&layers(&important)[0]), Color::rgb(0, 255, 0));
}

/// A stylesheet reaches every property the cascade carries, not just paints.
#[test]
fn a_stylesheet_may_set_any_property_the_cascade_holds() {
    let svg = document(
        r#"<style>.c{fill-rule:evenodd;opacity:0.5;clip-rule:evenodd}</style>
            <path class="c" d="M0 0 H8 V8 H0 Z M2 2 H6 V6 H2 Z"/>"#,
    );
    let decoded = tree(&svg);
    let [Node::Fill(layer)] = decoded.as_slice() else {
        panic!("expected one plain layer");
    };
    assert_eq!(layer.rule, FillRule::EvenOdd);
    assert_eq!(solid(layer).a, 128);
}

/// A `clip-path` this decoder cannot build is refused rather than drawn
/// without the clip the author asked for: a clean fallback beats a wrong
/// picture.
#[test]
fn a_clip_or_mask_value_outside_the_subset_refuses_the_document() {
    for value in ["inset(1px)", "circle(50%)", "url(other.svg#c)"] {
        let svg = document(&format!(
            r#"<rect width="8" height="8" fill="red" clip-path="{value}"/>"#
        ));
        assert_eq!(
            decode_square(svg.as_bytes()),
            Err(SvgError::InvalidReference),
            "{value}"
        );
    }
}

/// `mask-type` reaches the mask through the same cascade every other
/// property does, so a stylesheet may set it.
#[test]
fn a_stylesheet_may_choose_a_masks_channel() {
    let svg = document(
        r#"<style>#m{mask-type:alpha}</style>
           <mask id="m"><rect width="4" height="8" fill="white"/></mask>
           <rect width="8" height="8" fill="red" mask="url(#m)"/>"#,
    );
    assert_eq!(only_group(&svg).mask.expect("a mask").kind, MaskKind::Alpha);
}

/// `overflow` reaches a nested viewport through the cascade too.
#[test]
fn a_stylesheet_may_let_a_nested_viewport_spill() {
    let svg = r#"<svg viewBox="0 0 8 8"><style>svg{overflow:visible}</style>
        <svg width="4" height="8"><rect width="8" height="8" fill="red"/></svg></svg>"#;
    assert!(matches!(tree(svg).as_slice(), [Node::Fill(_)]));
}

/// A definition is reached by reference, so what it inherits is its own place
/// in the document — not the style of whatever element pointed at it.
#[test]
fn a_mask_inherits_from_its_own_ancestors_not_its_user() {
    // The mask's rect states no fill, so it takes the one its own ancestry
    // gives it: white from the root, not the referencing group's black.
    let svg = r#"<svg viewBox="0 0 8 8" fill="white">
        <mask id="m"><rect width="8" height="8"/></mask>
        <g fill="black"><rect width="8" height="8" fill="red" mask="url(#m)"/></g></svg>"#;
    let group = only_group(svg);
    let content = flatten(&group.mask.expect("a mask").content);
    assert_eq!(solid(&content[0]), Color::rgb(255, 255, 255));
}

#[test]
fn a_clip_inherits_its_rule_from_its_own_ancestors() {
    let svg = r#"<svg viewBox="0 0 8 8" clip-rule="evenodd">
        <clipPath id="c"><rect width="8" height="8"/></clipPath>
        <g clip-rule="nonzero"><rect width="8" height="8" fill="red" clip-path="url(#c)"/></g></svg>"#;
    let group = only_group(svg);
    let shapes = flatten(&group.mask.expect("a clip mask").content);
    assert_eq!(shapes[0].rule, FillRule::EvenOdd);
}

/// A stop's `currentColor` is the `color` the gradient's own ancestry gives
/// it, for the same reason a mask's content is.
#[test]
fn a_gradient_stops_current_color_comes_from_the_gradient_not_its_user() {
    let svg = r##"<svg viewBox="0 0 8 8" color="#00ff00">
        <linearGradient id="g"><stop offset="0" stop-color="currentColor"/>
          <stop offset="1" stop-color="currentColor"/></linearGradient>
        <g color="#ff0000"><rect width="8" height="8" fill="url(#g)"/></g></svg>"##;
    let decoded = layers(svg);
    let Paint::Gradient(gradient) = &decoded[0].paint else {
        panic!("expected a gradient paint");
    };
    for stop in &gradient.stops {
        assert_eq!(stop.color, Color::rgb(0, 255, 0));
    }
}

// --- patterns -------------------------------------------------------------

/// The pattern a document's only layer fills with.
#[track_caller]
fn only_pattern(svg: &str) -> Pattern {
    let image = decode_square(svg.as_bytes()).expect("a decodable document");
    let [Node::Fill(layer)] = image.nodes() else {
        panic!("expected one plain layer, got {:?}", image.nodes());
    };
    match &layer.paint {
        Paint::Pattern(pattern) => pattern.clone(),
        other => panic!("expected a pattern, got {other:?}"),
    }
}

/// One row of a document rasterised `side` pixels square, `#` where the
/// pixel is more than half opaque.
#[track_caller]
fn rendered_row(svg: &str, side: u32, row: u32) -> alloc::string::String {
    let image = decode_square(svg.as_bytes()).expect("a decodable document");
    let mut surface = Surface::new(side, side).expect("a small surface");
    assert!(
        surface.draw_artwork(image.nodes(), image.design()),
        "the renderer refused artwork the decoder accepted"
    );
    (0..side)
        .map(|x| {
            if surface.get(x, row).map_or(0, |pixel| pixel.a) > 128 {
                '#'
            } else {
                '.'
            }
        })
        .collect()
}

/// A tile stated in user space repeats across the shape at its own period,
/// and the tile's own content is clipped to it.
#[test]
fn a_user_space_tile_repeats_at_its_stated_period() {
    let svg = document(
        r##"<pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse">
              <rect width="2" height="4" fill="#f00"/></pattern>
            <rect width="8" height="8" fill="url(#p)"/>"##,
    );
    // Eight user units across sixteen pixels: the tile is eight pixels and
    // its half-covering stripe four.
    assert_eq!(rendered_row(&svg, 16, 8), "####....####....");
}

/// Bounding-box units are the initial ones, and they are fractions of the
/// filled shape rather than of the document.
#[test]
fn a_bounding_box_tile_is_a_fraction_of_the_shape() {
    let svg = document(
        r##"<pattern id="p" width="0.5" height="0.5">
              <rect width="1" height="4" fill="#f00"/></pattern>
            <rect x="0" y="0" width="4" height="4" fill="url(#p)"/>"##,
    );
    // The shape is four of the document's eight units, so half of it is a
    // two-unit tile — four pixels of a sixteen-pixel render — and the tile's
    // own one-unit stripe covers half of each repeat.
    assert_eq!(rendered_row(&svg, 16, 2), "##..##..........");
}

/// A `patternTransform` moves the tiling lattice, not just the content.
#[test]
fn a_pattern_transform_places_the_lattice() {
    let plain = only_pattern(&document(
        r##"<pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse">
              <rect width="2" height="4" fill="#f00"/></pattern>
            <rect width="8" height="8" fill="url(#p)"/>"##,
    ));
    let shifted = only_pattern(&document(
        r##"<pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse"
              patternTransform="translate(2 0)">
              <rect width="2" height="4" fill="#f00"/></pattern>
            <rect width="8" height="8" fill="url(#p)"/>"##,
    ));
    // Half a tile on: the same point in the drawing lands half a tile later.
    let at = (0.0, 0.0);
    assert_eq!(plain.tile_position(at), Some((0.0, 0.0)));
    assert_eq!(shifted.tile_position(at), Some((0.5, 0.0)));
}

/// `patternContentUnits` states the content in fractions of the filled
/// shape's box instead of in user space.
#[test]
fn pattern_content_units_scale_the_tile_content() {
    let svg = document(
        r##"<pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse"
              patternContentUnits="objectBoundingBox">
              <rect width="0.25" height="1" fill="#f00"/></pattern>
            <rect width="8" height="8" fill="url(#p)"/>"##,
    );
    // A quarter of the eight-unit box is two user units, which is half of
    // the four-unit tile: the same stripe the user-space case draws.
    assert_eq!(rendered_row(&svg, 16, 8), "####....####....");
}

/// A pattern's own `viewBox` fits its content to the tile, which is what
/// lets one drawing tile at any period.
#[test]
fn a_pattern_view_box_fits_its_content_to_the_tile() {
    let svg = document(
        r##"<pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse"
              viewBox="0 0 10 10" preserveAspectRatio="none">
              <rect width="5" height="10" fill="#f00"/></pattern>
            <rect width="8" height="8" fill="url(#p)"/>"##,
    );
    assert_eq!(rendered_row(&svg, 16, 8), "####....####....");
}

/// `href` inherits a pattern's content as well as its attributes.
#[test]
fn a_pattern_inherits_the_content_it_references() {
    let svg = document(
        r##"<pattern id="base" width="4" height="4" patternUnits="userSpaceOnUse">
              <rect width="2" height="4" fill="#f00"/></pattern>
            <pattern id="p" href="#base"/>
            <rect width="8" height="8" fill="url(#p)"/>"##,
    );
    assert_eq!(rendered_row(&svg, 16, 8), "####....####....");
}

/// A pattern that paints nothing is `none`, not the fallback colour beside
/// the reference: the reference was valid and the author's answer was empty.
#[test]
fn an_empty_pattern_paints_nothing_rather_than_the_fallback() {
    for pattern in [
        r#"<pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse"/>"#,
        // A zero extent disables the pattern, which SVG states outright.
        r##"<pattern id="p" width="0" height="4" patternUnits="userSpaceOnUse">
              <rect width="2" height="4" fill="#f00"/></pattern>"##,
    ] {
        let svg = document(&format!(
            r#"{pattern}<rect width="8" height="8" fill="url(#p) #00f"/>"#
        ));
        assert!(
            layers(&svg).is_empty(),
            "an empty pattern should paint nothing: {pattern}"
        );
    }
}

/// The same distinction for a gradient: no stops is `none`, while a name the
/// document never defines is what the fallback is for.
#[test]
fn a_stopless_gradient_paints_nothing_but_a_missing_one_falls_back() {
    let stopless =
        document(r#"<linearGradient id="g"/><rect width="8" height="8" fill="url(#g) #00f"/>"#);
    assert!(layers(&stopless).is_empty());

    let missing = document(r#"<rect width="8" height="8" fill="url(#nothing) #00f"/>"#);
    assert_eq!(solid(&layers(&missing)[0]), Color::rgb(0, 0, 255));
}

/// A tile whose content an author asked to spill past it draws that spill in
/// the neighbouring repeats, rather than being clipped to a picture the
/// author did not draw.
///
/// The tile is four user units of an eight-unit document over sixteen
/// pixels, so one repeat is eight pixels and one user unit is two. Its stripe
/// covers user 3..5 — inside the tile from 3, and one unit past it — so each
/// repeat shows its own stripe *and* the unit its left-hand neighbour spills
/// in.
#[test]
fn a_tile_that_overflows_its_own_bounds_spills_into_its_neighbours() {
    let spilling = document(
        r##"<pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse"
              overflow="visible">
              <rect x="3" width="2" height="4" fill="#f00"/></pattern>
            <rect width="8" height="8" fill="url(#p)"/>"##,
    );
    assert_eq!(rendered_row(&spilling, 16, 8), "##....####....##");

    // The same tile confined: only the unit inside it survives.
    let hidden = document(
        r##"<pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse">
              <rect x="3" width="2" height="4" fill="#f00"/></pattern>
            <rect width="8" height="8" fill="url(#p)"/>"##,
    );
    assert_eq!(rendered_row(&hidden, 16, 8), "......##......##");
}

/// Which side a spill needs a replica on is the opposite of the side it
/// spills toward: content past the tile's right edge is what the replica to
/// its *left* brings back in.
#[test]
fn a_spill_is_folded_from_the_side_it_reaches_back_from() {
    let rightward = only_pattern(&document(
        r##"<pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse"
              overflow="visible">
              <rect x="3" width="2" height="4" fill="#f00"/></pattern>
            <rect width="8" height="8" fill="url(#p)"/>"##,
    ));
    assert_eq!(
        rightward.fold,
        TileFold {
            before: (1, 0),
            after: (0, 0)
        }
    );

    let leftward = only_pattern(&document(
        r##"<pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse"
              overflow="visible">
              <rect x="-1" width="2" height="4" fill="#f00"/></pattern>
            <rect width="8" height="8" fill="url(#p)"/>"##,
    ));
    assert_eq!(
        leftward.fold,
        TileFold {
            before: (0, 0),
            after: (1, 0)
        }
    );
}

/// An `overflow` that cuts nothing off folds nothing, so a tile inside its
/// own bounds decodes and draws exactly as a confined one.
#[test]
fn an_overflow_that_spills_nothing_folds_nothing() {
    let inside = document(
        r##"<pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse"
              overflow="visible">
              <rect width="2" height="4" fill="#f00"/></pattern>
            <rect width="8" height="8" fill="url(#p)"/>"##,
    );
    assert_eq!(only_pattern(&inside).fold, TileFold::default());
    assert_eq!(rendered_row(&inside, 16, 8), "####....####....");

    let confined = document(
        r##"<pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse">
              <rect width="2" height="4" fill="#f00"/></pattern>
            <rect width="8" height="8" fill="url(#p)"/>"##,
    );
    assert_eq!(only_pattern(&confined).fold, TileFold::default());
}

/// Overlapping replicas draw in raster order, so the replica to the right
/// paints over the one to its left.
///
/// The tile's blue band sits one period along from its red one, so each
/// replica's blue lands exactly where the next replica's red does. Red is
/// half-transparent, so which of the two is on top is the difference between
/// a reddish blend and flat blue.
#[test]
fn overlapping_replicas_draw_in_raster_order() {
    let svg = document(
        r##"<pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse"
              overflow="visible">
              <rect width="2" height="4" fill="#f00" fill-opacity="0.5"/>
              <rect x="4" width="2" height="4" fill="#00f"/></pattern>
            <rect width="8" height="8" fill="url(#p)"/>"##,
    );
    let image = decode_square(svg.as_bytes()).expect("a decodable document");
    let mut surface = Surface::new(16, 16).expect("a small surface");
    assert!(surface.draw_artwork(image.nodes(), image.design()));

    let pixel = surface.get(2, 8).expect("an interior pixel");
    assert!(
        pixel.r > pixel.b,
        "the later replica's red should sit over the earlier one's blue, got {pixel:?}"
    );
}

/// The fold is bounded: content reaching more than a period past its tile is
/// a budget overrun, refused like any other rather than folded.
#[test]
fn a_spill_past_the_fold_bound_is_refused() {
    // Three periods of content — one past the tile on each side — is the
    // widest tile the bound admits.
    let admitted = document(
        r##"<pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse"
              overflow="visible">
              <rect x="-4" width="12" height="4" fill="#f00"/></pattern>
            <rect width="8" height="8" fill="url(#p)"/>"##,
    );
    assert_eq!(
        only_pattern(&admitted).fold,
        TileFold {
            before: (1, 0),
            after: (1, 0)
        }
    );

    let refused = document(
        r##"<pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse"
              overflow="visible">
              <rect width="10" height="4" fill="#f00"/></pattern>
            <rect width="8" height="8" fill="url(#p)"/>"##,
    );
    assert_eq!(decode_square(refused.as_bytes()), Err(SvgError::TooComplex));
}

/// A pattern's tile inherits from where the pattern sits, not from the shape
/// that used it — the rule every referenced definition follows.
#[test]
fn a_tile_inherits_from_its_own_place_in_the_document() {
    let svg = document(
        r##"<g fill="#f00"><pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse">
              <rect width="2" height="4"/></pattern></g>
            <rect width="8" height="8" fill="url(#p)" color="#0f0"/>"##,
    );
    let pattern = only_pattern(&svg);
    let [Node::Fill(layer)] = pattern.content.as_slice() else {
        panic!("expected one tile layer, got {:?}", pattern.content);
    };
    assert_eq!(solid(layer), Color::rgb(255, 0, 0));
}

/// A fill opacity weakens the assembled tile, not each of its parts: SVG
/// weakens the fill operation as a whole, so neither two overlapping layers
/// nor two overlapping replicas may each pay it.
#[test]
fn a_fill_opacity_weakens_the_assembled_tile() {
    let pattern = only_pattern(&document(
        r##"<pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse">
              <rect width="2" height="4" fill="#f00"/>
              <rect width="4" height="2" fill="#0f0"/></pattern>
            <rect width="8" height="8" fill="url(#p)" fill-opacity="0.5"/>"##,
    ));
    assert_eq!(pattern.opacity, 128);
    assert_eq!(pattern.content.len(), 2, "the layers stay unweakened");

    // Two periods of opaque content, so every pixel is covered by two
    // overlapping replicas. Weakening each one would composite to 191.
    let doubled = document(
        r##"<pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse"
              overflow="visible">
              <rect width="8" height="4" fill="#f00"/></pattern>
            <rect width="8" height="8" fill="url(#p)" fill-opacity="0.5"/>"##,
    );
    let image = decode_square(doubled.as_bytes()).expect("a decodable document");
    let mut surface = Surface::new(16, 16).expect("a small surface");
    assert!(surface.draw_artwork(image.nodes(), image.design()));
    assert_eq!(surface.get(8, 8).map(|pixel| pixel.a), Some(128));
}

/// A tile is a buffer in flight exactly as a group is, so the two share one
/// nesting bound and a document past it is refused rather than decoding into
/// artwork the renderer would turn away.
#[test]
fn patterns_nested_past_the_composite_bound_are_refused() {
    let mut body = alloc::string::String::from(
        r##"<pattern id="p0" width="4" height="4" patternUnits="userSpaceOnUse">
              <rect width="2" height="4" fill="#f00"/></pattern>"##,
    );
    for level in 1..=DESIGN_NESTING {
        let _ = write!(
            body,
            r#"<pattern id="p{level}" width="4" height="4" patternUnits="userSpaceOnUse">
                  <rect width="4" height="4" fill="url(#p{})"/></pattern>"#,
            level - 1
        );
    }
    let _ = write!(
        body,
        r#"<rect width="8" height="8" fill="url(#p{DESIGN_NESTING})"/>"#
    );
    assert_eq!(
        decode_square(document(&body).as_bytes()),
        Err(SvgError::TooComplex)
    );
}

/// A pattern that references itself is a cycle the nesting bound ends.
#[test]
fn a_self_referential_pattern_is_refused() {
    let svg = document(
        r#"<pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse">
              <rect width="4" height="4" fill="url(#p)"/></pattern>
            <rect width="8" height="8" fill="url(#p)"/>"#,
    );
    assert_eq!(decode_square(svg.as_bytes()), Err(SvgError::TooComplex));
}

/// A tile is a drawing in its own space, so its geometry is no part of the
/// bounding box of the shape being filled: a bounding-box clip on a
/// patterned shape is sized from the shape alone.
#[test]
fn a_tile_does_not_reach_the_filled_shapes_bounding_box() {
    let group = only_group(&document(
        r##"<clipPath id="c" clipPathUnits="objectBoundingBox">
              <rect width="0.5" height="1"/></clipPath>
            <pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse">
              <rect x="900" y="900" width="1" height="1" fill="#f00"/></pattern>
            <rect width="8" height="8" fill="url(#p)" clip-path="url(#c)"/>"##,
    ));
    let mask = group.mask.expect("a clip mask");
    let shapes = flatten(&mask.content);
    assert_eq!(box_of(&shapes[0]), (0, 0, 4 * UNIT, 8 * UNIT));
}

/// A tile establishes a viewport of its own, so a percentage in its content
/// is a fraction of the tile rather than of the document.
#[test]
fn a_tile_percentage_resolves_against_the_tile() {
    let svg = document(
        r##"<pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse"
              viewBox="0 0 10 10" preserveAspectRatio="none">
              <rect width="50%" height="100%" fill="#f00"/></pattern>
            <rect width="8" height="8" fill="url(#p)"/>"##,
    );
    assert_eq!(rendered_row(&svg, 16, 8), "####....####....");
}

/// A pattern's own geometry is charged against the document's vertex budget
/// exactly once, however the shape that uses it is composited.
#[test]
fn a_tile_is_charged_against_the_budget_once() {
    let svg = document(
        r##"<pattern id="p" width="4" height="4" patternUnits="userSpaceOnUse">
              <circle cx="2" cy="2" r="1" fill="#f00"/></pattern>
            <rect width="8" height="8" fill="url(#p)" stroke="#00f" stroke-width="1"
              opacity="0.5"/>"##,
    );
    let image = decode_square(svg.as_bytes()).expect("a decodable document");
    let [Node::Group(group)] = image.nodes() else {
        panic!("expected one composited element, got {:?}", image.nodes());
    };
    let [Node::Fill(fill), Node::Fill(_stroke)] = group.children.as_slice() else {
        panic!("expected a fill and a stroke, got {:?}", group.children);
    };
    let Paint::Pattern(pattern) = &fill.paint else {
        panic!("expected a pattern, got {:?}", fill.paint);
    };
    assert_eq!(pattern.content.len(), 1, "the tile is built exactly once");
}

// --- curve flattening ------------------------------------------------------

/// A curve is flattened to the accuracy the *design grid* needs, so the
/// placement it is drawn under decides the tolerance — not the document's own
/// root map. The same arc drawn directly and drawn ten times smaller inside
/// `scale(10)` reaches the grid at the same size, so it must be subdivided the
/// same; taking the root's tolerance would facet the scaled one tenfold.
#[test]
fn a_scaled_subtree_is_flattened_as_finely_as_the_grid_needs() {
    let direct = layers(r#"<svg viewBox="0 0 80 80"><path d="M0 40 A40 40 0 0 1 80 40"/></svg>"#);
    let scaled = layers(
        r#"<svg viewBox="0 0 80 80"><g transform="scale(10)">
             <path d="M0 4 A4 4 0 0 1 8 4"/></g></svg>"#,
    );
    assert_eq!(direct[0].vertices(), scaled[0].vertices());
}

/// A placement that collapses draws a point however finely a curve on it is
/// subdivided, so it takes the coarsest tolerance rather than an infinite one
/// the flattener would read as the finest and spend the whole vertex budget
/// on.
#[test]
fn a_collapsed_placement_does_not_subdivide_a_curve() {
    let svg =
        document(r#"<g transform="scale(0)"><path d="M0 4 A4 4 0 0 1 8 4 A4 4 0 0 1 0 4 Z"/></g>"#);
    let image = decode_square(svg.as_bytes()).expect("a decodable document");
    assert!(
        flatten(image.nodes())
            .iter()
            .all(|layer| layer.vertices() <= 8),
        "a collapsed arc should not be subdivided"
    );
}

// --- markers ---------------------------------------------------------------

/// A marker whose content is the unit square, so a test reads straight off
/// the contour where the instance landed and which way it faces.
fn unit_marker(id: &str, ink: &str, extra: &str) -> alloc::string::String {
    format!(
        r#"<marker id="{id}" markerWidth="1" markerHeight="1"
             markerUnits="userSpaceOnUse" {extra}>
             <rect width="1" height="1" fill="{ink}"/></marker>"#
    )
}

/// One user unit as a design coordinate pair.
fn at(x: f64, y: f64) -> (i32, i32) {
    let unit = f64::from(UNIT);
    (
        tairix_util::mathf::round_i32(x * unit),
        tairix_util::mathf::round_i32(y * unit),
    )
}

/// The start marker goes on the first vertex, the end marker on the last, and
/// the mid marker on everything between — and they paint in that order.
#[test]
fn markers_are_drawn_at_the_first_the_middle_and_the_last_vertex() {
    let svg = document(&format!(
        r#"{}{}{}<polyline points="1,1 3,1 5,1" fill="none"
             marker-start="url(#s)" marker-mid="url(#m)" marker-end="url(#e)"/>"#,
        unit_marker("s", "#ff0000", ""),
        unit_marker("m", "#00ff00", ""),
        unit_marker("e", "#0000ff", ""),
    ));
    let decoded = layers(&svg);
    assert_eq!(decoded.len(), 3);
    assert_eq!(solid(&decoded[0]), Color::rgb(255, 0, 0));
    assert_eq!(solid(&decoded[1]), Color::rgb(0, 255, 0));
    assert_eq!(solid(&decoded[2]), Color::rgb(0, 0, 255));
    assert_eq!(contour(&decoded[0])[0], at(1.0, 1.0));
    assert_eq!(contour(&decoded[1])[0], at(3.0, 1.0));
    assert_eq!(contour(&decoded[2])[0], at(5.0, 1.0));
}

/// A closed sub-path returns to where it began, so its first and last
/// vertices are the same point and carry both end markers — with the mids on
/// everything between, the corner the closure makes included.
#[test]
fn a_closed_sub_path_carries_both_end_markers_on_one_point() {
    let svg = document(&format!(
        r#"{}{}{}<polygon points="1,1 3,1 3,3 1,3" fill="none"
             marker-start="url(#s)" marker-mid="url(#m)" marker-end="url(#e)"/>"#,
        unit_marker("s", "#ff0000", ""),
        unit_marker("m", "#00ff00", ""),
        unit_marker("e", "#0000ff", ""),
    ));
    let decoded = layers(&svg);
    assert_eq!(decoded.len(), 5);
    assert_eq!(solid(&decoded[0]), Color::rgb(255, 0, 0));
    for mid in &decoded[1..4] {
        assert_eq!(solid(mid), Color::rgb(0, 255, 0));
    }
    assert_eq!(solid(&decoded[4]), Color::rgb(0, 0, 255));
    // The start and the end sit on the one point the closure returns to.
    assert_eq!(contour(&decoded[0])[0], at(1.0, 1.0));
    assert_eq!(contour(&decoded[4])[0], at(1.0, 1.0));
    assert_eq!(contour(&decoded[3])[0], at(1.0, 3.0));
}

/// A path of a single vertex is both the first and the last, so both end
/// markers land on it and no mid does.
#[test]
fn a_single_vertex_carries_the_start_and_the_end_marker() {
    let svg = document(&format!(
        r#"{}{}{}<path d="M2 2" marker-start="url(#s)" marker-mid="url(#m)"
             marker-end="url(#e)"/>"#,
        unit_marker("s", "#ff0000", ""),
        unit_marker("m", "#00ff00", ""),
        unit_marker("e", "#0000ff", ""),
    ));
    let decoded = layers(&svg);
    assert_eq!(decoded.len(), 2);
    assert_eq!(solid(&decoded[0]), Color::rgb(255, 0, 0));
    assert_eq!(solid(&decoded[1]), Color::rgb(0, 0, 255));
}

/// `orient="auto"` turns an instance to follow the path, and at a corner it
/// takes the bisector of what arrives and what leaves.
#[test]
fn an_auto_oriented_marker_bisects_the_corner_it_sits_on() {
    let svg = document(&format!(
        r#"{}<polyline points="1,1 3,1 3,3" fill="none" marker-mid="url(#m)"/>"#,
        unit_marker("m", "#000000", r#"orient="auto" overflow="visible""#),
    ));
    let decoded = layers(&svg);
    assert_eq!(decoded.len(), 1);
    let half = core::f64::consts::FRAC_1_SQRT_2;
    assert_eq!(contour(&decoded[0])[0], at(3.0, 1.0));
    // The marker's own positive x axis, one unit along the 45° bisector.
    assert_eq!(contour(&decoded[0])[1], at(3.0 + half, 1.0 + half));
}

/// `auto-start-reverse` turns the start marker about and leaves every other
/// one alone, which is what lets one arrowhead point out of both ends.
#[test]
fn auto_start_reverse_turns_only_the_start_marker_about() {
    let svg = document(&format!(
        r#"{}<polyline points="1,1 3,1" fill="none"
             marker-start="url(#m)" marker-end="url(#m)"/>"#,
        unit_marker(
            "m",
            "#000000",
            r#"orient="auto-start-reverse" overflow="visible""#
        ),
    ));
    let decoded = layers(&svg);
    assert_eq!(decoded.len(), 2);
    assert_eq!(contour(&decoded[0])[1], at(0.0, 1.0));
    assert_eq!(contour(&decoded[1])[1], at(4.0, 1.0));
}

/// A stated angle ignores the path entirely.
#[test]
fn a_stated_orient_angle_ignores_the_path() {
    let svg = document(&format!(
        r#"{}<polyline points="1,1 3,1" fill="none" marker-start="url(#m)"/>"#,
        unit_marker("m", "#000000", r#"orient="90" overflow="visible""#),
    ));
    let decoded = layers(&svg);
    assert_eq!(contour(&decoded[0])[1], at(1.0, 2.0));
}

/// The default units measure a marker in the referencing element's stroke
/// widths, so it grows with the line; `userSpaceOnUse` leaves it alone.
#[test]
fn stroke_width_units_scale_a_marker_and_user_space_units_do_not() {
    let scaled = document(
        r##"<marker id="m" markerWidth="1" markerHeight="1" overflow="visible">
              <rect width="1" height="1" fill="#000000"/></marker>
            <polyline points="1,1 3,1 3,3" fill="none" stroke="#123456" stroke-width="2"
              marker-start="url(#m)"/>"##,
    );
    let decoded = layers(&scaled);
    // The stroke, then the marker over it.
    assert_eq!(decoded.len(), 2);
    assert_eq!(contour(&decoded[1])[0], at(1.0, 1.0));
    assert_eq!(contour(&decoded[1])[1], at(3.0, 1.0));

    let fixed = document(
        r##"<marker id="m" markerWidth="1" markerHeight="1" markerUnits="userSpaceOnUse"
              overflow="visible"><rect width="1" height="1" fill="#000000"/></marker>
            <polyline points="1,1 3,1 3,3" fill="none" stroke="#123456" stroke-width="2"
              marker-start="url(#m)"/>"##,
    );
    assert_eq!(contour(&layers(&fixed)[1])[1], at(2.0, 1.0));
}

/// `refX`/`refY` name the content point that lands on the vertex.
#[test]
fn the_reference_point_sits_on_the_vertex() {
    let svg = document(
        r##"<marker id="m" markerWidth="2" markerHeight="2" markerUnits="userSpaceOnUse"
              refX="1" refY="1"><rect width="2" height="2" fill="#000000"/></marker>
            <polyline points="4,4 6,4" fill="none" marker-start="url(#m)"/>"##,
    );
    let decoded = layers(&svg);
    assert_eq!(contour(&decoded[0])[0], at(3.0, 3.0));
    assert_eq!(contour(&decoded[0])[2], at(5.0, 5.0));
}

/// A marker's own `viewBox` fits its content to the viewport, so the drawing
/// is authored in whatever coordinates suit it.
#[test]
fn a_marker_view_box_fits_its_content_to_the_viewport() {
    let svg = document(
        r##"<marker id="m" markerWidth="2" markerHeight="2" markerUnits="userSpaceOnUse"
              viewBox="0 0 4 4"><rect width="4" height="4" fill="#000000"/></marker>
            <polyline points="1,1 3,1" fill="none" marker-start="url(#m)"/>"##,
    );
    let decoded = layers(&svg);
    assert_eq!(contour(&decoded[0])[0], at(1.0, 1.0));
    assert_eq!(contour(&decoded[0])[2], at(3.0, 3.0));
}

/// A marker viewport clips its content like any other — but content already
/// inside it composites identically without a group, so the isolation buffer
/// is allocated only where the viewport actually cuts something.
#[test]
fn a_marker_clips_to_its_viewport_only_where_it_must() {
    let fits = tree(&document(&format!(
        r#"{}<polyline points="1,1 3,1" fill="none" marker-start="url(#m)"/>"#,
        unit_marker("m", "#000000", ""),
    )));
    assert!(matches!(fits.as_slice(), [Node::Fill(_)]), "{fits:?}");

    let spills = document(
        r##"<marker id="m" markerWidth="1" markerHeight="1" markerUnits="userSpaceOnUse">
              <rect width="4" height="4" fill="#000000"/></marker>
            <polyline points="1,1 3,1" fill="none" marker-start="url(#m)"/>"##,
    );
    let clipped = only_group(&spills);
    assert!(clipped.mask.is_some());
    assert_eq!(clipped.opacity, u8::MAX);

    // The same spill drawn where the author asked for no clip.
    let visible = document(
        r##"<marker id="m" markerWidth="1" markerHeight="1" markerUnits="userSpaceOnUse"
              overflow="visible"><rect width="4" height="4" fill="#000000"/></marker>
            <polyline points="1,1 3,1" fill="none" marker-start="url(#m)"/>"##,
    );
    assert!(matches!(tree(&visible).as_slice(), [Node::Fill(_)]));
}

/// `paint-order` reaches the markers too, not just the fill and the stroke.
#[test]
fn paint_order_moves_the_markers_with_the_other_two() {
    let body = format!(
        r##"{}<polyline points="1,1 3,1 3,3" fill="#ff0000" stroke="#0000ff"
             stroke-width="1" marker-start="url(#m)""##,
        unit_marker("m", "#00ff00", ""),
    );
    let normal = layers(&document(&format!("{body}/>")));
    assert_eq!(solid(&normal[0]), Color::rgb(255, 0, 0));
    assert_eq!(solid(&normal[1]), Color::rgb(0, 0, 255));
    assert_eq!(solid(&normal[2]), Color::rgb(0, 255, 0));

    let reordered = layers(&document(&format!(
        r#"{body} paint-order="markers fill stroke"/>"#
    )));
    assert_eq!(solid(&reordered[0]), Color::rgb(0, 255, 0));
    assert_eq!(solid(&reordered[1]), Color::rgb(255, 0, 0));
    assert_eq!(solid(&reordered[2]), Color::rgb(0, 0, 255));
}

/// A marker takes its style from its own place in the document, not from the
/// shape that placed it — there is no way in SVG 1.1 for a marker to be
/// tinted by its user.
#[test]
fn a_marker_takes_its_own_place_in_the_document() {
    let svg = document(
        r##"<g fill="#ff0000"><marker id="m" markerWidth="1" markerHeight="1"
              markerUnits="userSpaceOnUse"><rect width="1" height="1"/></marker></g>
            <polyline points="1,1 3,1" fill="none" marker-start="url(#m)"/>"##,
    );
    let decoded = layers(&svg);
    assert_eq!(decoded.len(), 1);
    assert_eq!(solid(&decoded[0]), Color::rgb(255, 0, 0));
}

/// A `<marker>` is drawn only where it is referenced, never where it sits.
#[test]
fn a_marker_is_not_drawn_where_it_is_defined() {
    let svg = document(&format!(
        r##"{}<rect width="2" height="2" fill="#000000"/>"##,
        unit_marker("m", "#ff0000", ""),
    ));
    assert_eq!(layers(&svg).len(), 1);
}

/// A reference to a marker the document does not define draws nothing and
/// leaves the shape alone — a missing decoration cannot show more than the
/// author asked for, so there is nothing to fail closed against.
#[test]
fn a_dangling_marker_reference_draws_nothing_and_keeps_the_shape() {
    let svg = document(
        r##"<polyline points="1,1 3,1 3,3" fill="#ff0000" marker-start="url(#nothing)"
              marker-mid="url(#nothing)" marker-end="url(#nothing)"/>"##,
    );
    let decoded = layers(&svg);
    assert_eq!(decoded.len(), 1);
    assert_eq!(solid(&decoded[0]), Color::rgb(255, 0, 0));
}

/// Markers go on the shapes whose vertices the author wrote. A rectangle,
/// circle, or ellipse has none of its own — its outline is the decoder's
/// flattening — so it carries none.
#[test]
fn markers_are_ignored_on_the_shapes_that_have_no_authored_vertices() {
    for shape in [
        r##"<rect width="4" height="4" fill="#ff0000""##,
        r##"<circle cx="4" cy="4" r="2" fill="#ff0000""##,
        r##"<ellipse cx="4" cy="4" rx="2" ry="1" fill="#ff0000""##,
    ] {
        let svg = document(&format!(
            r#"{}{shape} marker-start="url(#m)" marker-mid="url(#m)"
                 marker-end="url(#m)"/>"#,
            unit_marker("m", "#00ff00", ""),
        ));
        let decoded = layers(&svg);
        assert_eq!(decoded.len(), 1, "{shape}");
        assert_eq!(solid(&decoded[0]), Color::rgb(255, 0, 0), "{shape}");
    }
}

/// An element's own opacity composites its markers with the rest of it as one
/// unit: folding it into each would show the shape through its own
/// decorations.
#[test]
fn an_elements_opacity_composites_its_markers_with_it() {
    let svg = document(&format!(
        r#"{}<polyline points="1,1 3,1" fill="none" opacity="0.5"
             marker-start="url(#m)"/>"#,
        unit_marker("m", "#000000", ""),
    ));
    let group = only_group(&svg);
    assert!(group.opacity < u8::MAX);
    assert!(group.mask.is_none());
    assert_eq!(flatten(&group.children).len(), 1);
}

/// One instance is one element visit, so a marker placed at every vertex of a
/// long path is bounded by the work it asks for — even when its content
/// resolves to no paint at all and so charges no layer and no vertex.
#[test]
fn too_many_marker_instances_is_refused() {
    let content = "<rect width=\"1\" height=\"1\" fill=\"none\"/>".repeat(20);
    let marked = |points: usize| {
        let mut list = alloc::string::String::new();
        for index in 0..points {
            let _ = write!(list, "{},{} ", index % 7, index % 5);
        }
        document(&format!(
            r#"<marker id="m" markerWidth="1" markerHeight="1"
                  markerUnits="userSpaceOnUse">{content}</marker>
                <polyline points="{list}" fill="none" marker-mid="url(#m)"/>"#
        ))
    };
    // Few enough instances to afford, and drawing nothing, so no other budget
    // can be what turns the larger one away.
    let affordable = decode_square(marked(1000).as_bytes()).expect("an affordable document");
    assert_eq!(affordable.nodes().len(), 0);
    assert_eq!(
        decode_square(marked(4000).as_bytes()),
        Err(SvgError::TooComplex)
    );
}

/// A marker whose content places the same marker would never end; the visit
/// budget is what stops it, like every other reference cycle.
#[test]
fn a_marker_that_places_itself_terminates() {
    let svg = document(
        r#"<marker id="m" markerWidth="1" markerHeight="1" markerUnits="userSpaceOnUse">
              <polyline points="0,0 1,1" fill="none" marker-start="url(#m)"/></marker>
            <polyline points="1,1 3,1" fill="none" marker-start="url(#m)"/>"#,
    );
    assert_eq!(decode_square(svg.as_bytes()), Err(SvgError::TooComplex));
}

/// A definition's own style is memoised, because it is resolved once per
/// place it is referenced from — a marker, once per vertex. The viewport is
/// part of what it is memoised against: a percentage length on the
/// definition's ancestry resolves against whichever viewport the
/// *referencing* element sits in, so the same definition genuinely has two
/// answers and must not be handed the first one twice.
#[test]
fn a_definition_resolves_its_percentages_per_referencing_viewport() {
    let svg = r##"<svg viewBox="0 0 8 8">
        <g stroke-width="50%"><marker id="m" markerWidth="8" markerHeight="8"
          markerUnits="userSpaceOnUse" overflow="visible">
          <line x1="0" y1="0" x2="1" y2="0" stroke="#000000"/></marker></g>
        <polyline points="1,1 3,1" fill="none" marker-start="url(#m)"/>
        <svg x="0" y="4" width="4" height="4" viewBox="0 0 4 4" overflow="visible">
          <polyline points="1,1 3,1" fill="none" marker-start="url(#m)"/></svg>
        </svg>"##;
    let decoded = layers(svg);
    assert_eq!(decoded.len(), 2);
    let height = |layer: &Layer| {
        let (_, top, _, bottom) = box_of(layer);
        bottom - top
    };
    // The root viewport's diagonal is eight user units, the nested one's is
    // four, so the same `50%` stroke is twice as wide in the first.
    assert_eq!(height(&decoded[0]), 4 * UNIT);
    assert_eq!(height(&decoded[1]), 2 * UNIT);
}
