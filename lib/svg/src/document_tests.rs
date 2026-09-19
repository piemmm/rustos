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

use tairix_raster::{for_each_fill, Color, FillRule, Group, Layer, MaskKind, Node, Paint};

use crate::error::SvgError;
use crate::{decode, Viewport, DESIGN_GRID};

/// Fit a document to the square slot, which is what all but the viewport's
/// own tests are about.
fn decode_square(bytes: &[u8]) -> Result<crate::SvgImage, SvgError> {
    decode(bytes, Viewport::Square)
}

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
        Paint::Gradient(_) => panic!("expected a solid paint"),
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
