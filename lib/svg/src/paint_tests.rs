//! Unit tests for gradients and paint-server resolution.
//!
//! A gradient's whole point is what colour lands where, so these tests go
//! through the real decode and then *sample* the resulting paint at
//! design-grid points, rather than asserting on the matrix that gets it
//! there.

use core::fmt::Write;

use tairix_raster::{Color, Paint};

use crate::error::SvgError;
use crate::{decode, DESIGN_GRID};

/// Design units per user unit: every document here has an eight-unit view
/// box, so a sample point is easy to state exactly.
fn scale() -> f64 {
    f64::from(DESIGN_GRID) / 8.0
}

/// The paint of a document's only layer.
#[track_caller]
fn only_paint(svg: &str) -> Paint {
    let image = decode(svg.as_bytes()).expect("a decodable document");
    assert_eq!(image.layers().len(), 1, "expected exactly one layer");
    image.layers()[0].paint.clone()
}

/// The colour a paint gives at a point in *user* coordinates.
#[track_caller]
fn sample(paint: &Paint, user: (f64, f64)) -> Color {
    paint.sample((user.0 * scale(), user.1 * scale()))
}

#[track_caller]
fn near(actual: Color, expected: Color) {
    let close = |a: u8, b: u8| i32::from(a).abs_diff(i32::from(b)) <= 2;
    assert!(
        close(actual.r, expected.r)
            && close(actual.g, expected.g)
            && close(actual.b, expected.b)
            && close(actual.a, expected.a),
        "expected {expected:?}, got {actual:?}"
    );
}

/// A document with `defs` and one filled rectangle covering the whole view
/// box.
fn document(defs: &str, fill: &str) -> alloc::string::String {
    alloc::format!(
        r#"<svg viewBox="0 0 8 8"><defs>{defs}</defs>
            <rect width="8" height="8" fill="{fill}"/></svg>"#
    )
}

// --- linear gradients -----------------------------------------------------

#[test]
fn a_linear_gradient_runs_between_its_stops() {
    let svg = document(
        r##"<linearGradient id="g">
              <stop offset="0" stop-color="#000000"/>
              <stop offset="1" stop-color="#ffffff"/>
            </linearGradient>"##,
        "url(#g)",
    );
    let paint = only_paint(&svg);
    near(sample(&paint, (0.0, 4.0)), Color::rgb(0, 0, 0));
    near(sample(&paint, (8.0, 4.0)), Color::rgb(255, 255, 255));
    near(sample(&paint, (4.0, 4.0)), Color::rgb(128, 128, 128));
}

/// Bounding-box units are fractions of the shape, so a gradient defined once
/// paints every shape that references it across its own extent.
#[test]
fn bounding_box_units_follow_the_shape() {
    let svg = r##"<svg viewBox="0 0 8 8"><defs>
              <linearGradient id="g" x1="0" y1="0" x2="1" y2="0">
                <stop offset="0" stop-color="#000000"/>
                <stop offset="1" stop-color="#ffffff"/>
              </linearGradient></defs>
            <rect x="4" y="0" width="4" height="8" fill="url(#g)"/></svg>"##;
    let paint = only_paint(svg);
    near(sample(&paint, (4.0, 4.0)), Color::rgb(0, 0, 0));
    near(sample(&paint, (8.0, 4.0)), Color::rgb(255, 255, 255));
}

#[test]
fn user_space_units_ignore_the_shape() {
    let svg = document(
        r##"<linearGradient id="g" gradientUnits="userSpaceOnUse" x1="0" y1="0" x2="8" y2="0">
              <stop offset="0" stop-color="#000000"/>
              <stop offset="1" stop-color="#ffffff"/>
            </linearGradient>"##,
        "url(#g)",
    );
    let paint = only_paint(&svg);
    near(sample(&paint, (0.0, 0.0)), Color::rgb(0, 0, 0));
    near(sample(&paint, (8.0, 0.0)), Color::rgb(255, 255, 255));
}

#[test]
fn a_vertical_gradient_runs_down_the_shape() {
    let svg = document(
        r##"<linearGradient id="g" x1="0" y1="0" x2="0" y2="1">
              <stop offset="0" stop-color="#000000"/>
              <stop offset="1" stop-color="#ffffff"/>
            </linearGradient>"##,
        "url(#g)",
    );
    let paint = only_paint(&svg);
    near(sample(&paint, (4.0, 0.0)), Color::rgb(0, 0, 0));
    near(sample(&paint, (4.0, 8.0)), Color::rgb(255, 255, 255));
}

#[test]
fn a_gradient_transform_moves_the_gradient_with_the_shape_still_in_place() {
    let svg = document(
        r##"<linearGradient id="g" gradientUnits="userSpaceOnUse"
                x1="0" y1="0" x2="4" y2="0"
                gradientTransform="translate(4 0)">
              <stop offset="0" stop-color="#000000"/>
              <stop offset="1" stop-color="#ffffff"/>
            </linearGradient>"##,
        "url(#g)",
    );
    let paint = only_paint(&svg);
    near(sample(&paint, (4.0, 0.0)), Color::rgb(0, 0, 0));
    near(sample(&paint, (8.0, 0.0)), Color::rgb(255, 255, 255));
}

// --- spread ---------------------------------------------------------------

#[test]
fn each_spread_method_treats_the_outside_differently() {
    let build = |spread: &str| {
        document(
            &alloc::format!(
                r##"<linearGradient id="g" x1="0.25" y1="0" x2="0.5" y2="0" spreadMethod="{spread}">
                      <stop offset="0" stop-color="#000000"/>
                      <stop offset="1" stop-color="#ffffff"/>
                    </linearGradient>"##
            ),
            "url(#g)",
        )
    };

    let padded = only_paint(&build("pad"));
    near(sample(&padded, (0.0, 4.0)), Color::rgb(0, 0, 0));
    near(sample(&padded, (8.0, 4.0)), Color::rgb(255, 255, 255));

    let repeated = only_paint(&build("repeat"));
    // A quarter past the end of the run is a quarter into the next copy.
    near(sample(&repeated, (4.5, 4.0)), Color::rgb(64, 64, 64));

    let reflected = only_paint(&build("reflect"));
    near(sample(&reflected, (4.5, 4.0)), Color::rgb(191, 191, 191));
}

// --- radial gradients -----------------------------------------------------

#[test]
fn a_radial_gradient_runs_out_from_its_centre() {
    let svg = document(
        r##"<radialGradient id="g">
              <stop offset="0" stop-color="#ffffff"/>
              <stop offset="1" stop-color="#000000"/>
            </radialGradient>"##,
        "url(#g)",
    );
    let paint = only_paint(&svg);
    near(sample(&paint, (4.0, 4.0)), Color::rgb(255, 255, 255));
    near(sample(&paint, (8.0, 4.0)), Color::rgb(0, 0, 0));
    near(sample(&paint, (4.0, 8.0)), Color::rgb(0, 0, 0));
}

#[test]
fn a_focal_point_shifts_where_a_radial_gradient_starts() {
    let svg = document(
        r##"<radialGradient id="g" cx="0.5" cy="0.5" r="0.5" fx="0.25" fy="0.5">
              <stop offset="0" stop-color="#ffffff"/>
              <stop offset="1" stop-color="#000000"/>
            </radialGradient>"##,
        "url(#g)",
    );
    let paint = only_paint(&svg);
    near(sample(&paint, (2.0, 4.0)), Color::rgb(255, 255, 255));
    assert!(sample(&paint, (4.0, 4.0)).r < 250, "the centre has moved");
}

// --- stops ----------------------------------------------------------------

#[test]
fn stop_opacity_and_the_elements_own_opacity_multiply() {
    let svg = document(
        r##"<linearGradient id="g">
              <stop offset="0" stop-color="#ffffff" stop-opacity="0.5"/>
              <stop offset="1" stop-color="#ffffff" stop-opacity="0.5"/>
            </linearGradient>"##,
        "url(#g)",
    );
    let paint = only_paint(&svg);
    assert_eq!(sample(&paint, (4.0, 4.0)).a, 128);
}

#[test]
fn a_stop_may_be_styled_and_may_use_a_percentage_offset() {
    let svg = document(
        r#"<linearGradient id="g">
              <stop offset="0%" style="stop-color:#000000"/>
              <stop offset="100%" style="stop-color:#ff0000;stop-opacity:1"/>
            </linearGradient>"#,
        "url(#g)",
    );
    let paint = only_paint(&svg);
    near(sample(&paint, (8.0, 4.0)), Color::rgb(255, 0, 0));
}

/// A stop offset that goes backwards is pulled up to its predecessor rather
/// than reordering the list.
#[test]
fn a_backward_stop_offset_is_pulled_forward() {
    let svg = document(
        r##"<linearGradient id="g">
              <stop offset="0.75" stop-color="#000000"/>
              <stop offset="0.25" stop-color="#ffffff"/>
            </linearGradient>"##,
        "url(#g)",
    );
    let paint = only_paint(&svg);
    near(sample(&paint, (2.0, 4.0)), Color::rgb(0, 0, 0));
    near(sample(&paint, (8.0, 4.0)), Color::rgb(255, 255, 255));
}

/// One gradient may inherit another's stops and geometry, which is how
/// artwork shares a palette between a light and a dark variant.
#[test]
fn a_gradient_inherits_from_the_one_it_references() {
    let svg = document(
        r##"<linearGradient id="base">
              <stop offset="0" stop-color="#000000"/>
              <stop offset="1" stop-color="#ffffff"/>
            </linearGradient>
            <linearGradient id="g" href="#base" x1="0" y1="0" x2="0" y2="1"/>"##,
        "url(#g)",
    );
    let paint = only_paint(&svg);
    near(sample(&paint, (4.0, 0.0)), Color::rgb(0, 0, 0));
    near(sample(&paint, (4.0, 8.0)), Color::rgb(255, 255, 255));
}

#[test]
fn the_svg_one_point_one_link_spelling_is_understood() {
    let svg = document(
        r##"<linearGradient id="base">
              <stop offset="0" stop-color="#ff0000"/>
              <stop offset="1" stop-color="#0000ff"/>
            </linearGradient>
            <linearGradient id="g" xlink:href="#base"/>"##,
        "url(#g)",
    );
    let paint = only_paint(&svg);
    near(sample(&paint, (0.0, 4.0)), Color::rgb(255, 0, 0));
}

/// A cycle of references must terminate rather than walking for ever.
#[test]
fn a_reference_cycle_terminates() {
    let svg = document(
        r##"<linearGradient id="a" href="#b"/>
            <linearGradient id="b" href="#a">
              <stop offset="0" stop-color="#00ff00"/>
              <stop offset="1" stop-color="#00ff00"/>
            </linearGradient>"##,
        "url(#a)",
    );
    let paint = only_paint(&svg);
    near(sample(&paint, (4.0, 4.0)), Color::rgb(0, 255, 0));
}

// --- degenerate and missing servers ---------------------------------------

/// A gradient with no extent has no direction to run along, so SVG paints the
/// last stop's colour over the whole shape.
#[test]
fn a_gradient_with_no_extent_paints_its_last_stop() {
    let svg = document(
        r##"<linearGradient id="g" x1="0.5" y1="0.5" x2="0.5" y2="0.5">
              <stop offset="0" stop-color="#000000"/>
              <stop offset="1" stop-color="#ff0000"/>
            </linearGradient>"##,
        "url(#g)",
    );
    assert_eq!(only_paint(&svg), Paint::Solid(Color::rgb(255, 0, 0)));
}

#[test]
fn a_gradient_with_no_stops_paints_nothing() {
    let svg = document(r#"<linearGradient id="g"/>"#, "url(#g)");
    let image = decode(svg.as_bytes()).expect("a decodable document");
    assert!(image.layers().is_empty());
}

#[test]
fn an_unresolvable_reference_falls_back_to_the_colour_beside_it() {
    let svg = document("", "url(#missing) #00ff00");
    assert_eq!(only_paint(&svg), Paint::Solid(Color::rgb(0, 255, 0)));

    let bare = document("", "url(#missing)");
    let image = decode(bare.as_bytes()).expect("a decodable document");
    assert!(image.layers().is_empty());
}

#[test]
fn an_unknown_spread_method_is_refused() {
    let svg = document(
        r##"<linearGradient id="g" spreadMethod="sideways">
              <stop offset="0" stop-color="#000000"/>
            </linearGradient>"##,
        "url(#g)",
    );
    assert_eq!(decode(svg.as_bytes()), Err(SvgError::InvalidNumber));
}

/// A gradient is a handful of stops in every real asset; an unbounded list
/// would make every sampled pixel walk it.
#[test]
fn an_unbounded_stop_list_is_refused() {
    let mut stops = alloc::string::String::new();
    for index in 0..200 {
        let offset = f64::from(index) / 200.0;
        let _ = write!(stops, r##"<stop offset="{offset}" stop-color="#000000"/>"##);
    }
    let svg = document(
        &alloc::format!(r#"<linearGradient id="g">{stops}</linearGradient>"#),
        "url(#g)",
    );
    assert_eq!(decode(svg.as_bytes()), Err(SvgError::TooComplex));
}

#[test]
fn a_gradient_may_stroke_as_well_as_fill() {
    let svg = r##"<svg viewBox="0 0 8 8"><defs>
              <linearGradient id="g">
                <stop offset="0" stop-color="#ff0000"/>
                <stop offset="1" stop-color="#0000ff"/>
              </linearGradient></defs>
            <rect x="1" y="1" width="6" height="6" fill="none"
                  stroke="url(#g)" stroke-width="1"/></svg>"##;
    let image = decode(svg.as_bytes()).expect("a stroked gradient");
    assert_eq!(image.layers().len(), 1);
    assert!(matches!(image.layers()[0].paint, Paint::Gradient(_)));
}
