//! Unit tests for the document walk: what a whole SVG decodes to.
//!
//! The decoder is the desktop's untrusted-asset parser, so these cover both
//! the happy path — each element and structure produces the right layers, in
//! the right order, at the right place — and the fail-closed path, where
//! every malformed or unaffordable document is a precise [`SvgError`] and
//! never a panic.

use alloc::format;
use alloc::vec::Vec;

use tairix_raster::{Color, FillRule, Paint};

use crate::error::SvgError;
use crate::{decode, SvgLayer, DESIGN_GRID};

/// Every test document uses an eight-unit view box, so one user unit is
/// exactly this many design units and every expected coordinate is a round
/// number.
const UNIT: i32 = 256;

/// The layers a document decodes to.
#[track_caller]
fn layers(svg: &str) -> Vec<SvgLayer> {
    decode(svg.as_bytes())
        .expect("a decodable document")
        .layers()
        .to_vec()
}

/// A document with `body` inside an eight-unit square view box.
fn document(body: &str) -> alloc::string::String {
    format!(r#"<svg viewBox="0 0 8 8">{body}</svg>"#)
}

/// The solid colour a layer paints with.
#[track_caller]
fn solid(layer: &SvgLayer) -> Color {
    match layer.paint {
        Paint::Solid(color) => color,
        Paint::Gradient(_) => panic!("expected a solid paint"),
    }
}

/// The one contour of a single-contour layer.
#[track_caller]
fn contour(layer: &SvgLayer) -> &[(i32, i32)] {
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
        let image = decode(svg.as_bytes()).expect("a decodable document");
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
    assert_eq!((left, right), (0, i32::try_from(DESIGN_GRID).unwrap_or(0)));
    assert_eq!(bottom - top, i32::try_from(DESIGN_GRID).unwrap_or(0) / 2);
    assert_eq!(top, i32::try_from(DESIGN_GRID).unwrap_or(0) / 4);
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
    assert_eq!(decode(svg.as_bytes()), Err(SvgError::TooComplex));
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
    let image = decode(svg.as_bytes()).expect("a decodable document");
    assert_eq!(image.hotspot(), Some((2 * UNIT, 3 * UNIT)));
}

#[test]
fn no_hotspot_is_none_and_half_a_hotspot_is_refused() {
    let svg = document(r#"<rect width="1" height="1"/>"#);
    assert_eq!(
        decode(svg.as_bytes())
            .expect("a decodable document")
            .hotspot(),
        None
    );
    let half = r#"<svg viewBox="0 0 8 8" data-hotspot-x="2"><rect width="1" height="1"/></svg>"#;
    assert_eq!(decode(half.as_bytes()), Err(SvgError::InvalidNumber));
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
    assert_eq!(decode(b"<html><body/></html>"), Err(SvgError::MissingRoot));
    assert_eq!(decode(b""), Err(SvgError::MissingRoot));
    assert_eq!(decode(&[0xff, 0xfe, 0xfd]), Err(SvgError::NotUtf8));
}

#[test]
fn a_document_with_no_coordinate_system_is_refused() {
    assert_eq!(
        decode(br#"<svg><rect width="1" height="1"/></svg>"#),
        Err(SvgError::MissingViewBox)
    );
    assert_eq!(
        decode(br#"<svg viewBox="0 0 0 8"><rect width="1" height="1"/></svg>"#),
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
            decode(bad.as_bytes()),
            Err(SvgError::Malformed),
            "{bad:?} should be refused"
        );
    }
}

#[test]
fn a_malformed_value_anywhere_refuses_the_whole_document() {
    assert_eq!(
        decode(document(r#"<rect width="1" height="1" fill="chartreuseish"/>"#).as_bytes()),
        Err(SvgError::InvalidColor)
    );
    assert_eq!(
        decode(document(r#"<rect width="1" height="1" transform="wobble(2)"/>"#).as_bytes()),
        Err(SvgError::InvalidNumber)
    );
    assert_eq!(
        decode(document(r#"<path d="M0 0 X1 1"/>"#).as_bytes()),
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
        decode(document(&deep).as_bytes()),
        Err(SvgError::TooComplex)
    );

    let many = "<rect width=\"1\" height=\"1\"/>".repeat(9000);
    assert_eq!(
        decode(document(&many).as_bytes()),
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
        if let Ok(image) = decode(case.as_bytes()) {
            for layer in image.layers() {
                assert!(!layer.contours.is_empty(), "{case:?} made an empty layer");
            }
        }
    }
}
