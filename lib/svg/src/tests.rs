//! Unit tests for the SVG decoder.
//!
//! The decoder is the desktop's untrusted-asset parser, so the tests cover
//! both the happy path (each supported shape and fill form decodes to the
//! right layers) and the fail-closed path (every malformed or out-of-subset
//! document is a precise [`SvgError`], never a panic).

use alloc::vec;

use rustos_raster::Color;

use crate::{decode, SvgError};

#[test]
fn decodes_a_single_filled_polygon() {
    let svg =
        br##"<svg viewBox="0 0 24 24"><polygon points="2,2 22,2 12,22" fill="#ff8800"/></svg>"##;
    let image = decode(svg).expect("valid polygon");
    assert_eq!(image.design(), 24);
    assert_eq!(image.layers().len(), 1);
    let layer = &image.layers()[0];
    assert_eq!(layer.fill, Color::rgb(0xff, 0x88, 0x00));
    assert_eq!(layer.polygon, vec![(2, 2), (22, 2), (12, 22)]);
}

#[test]
fn layers_keep_document_order_for_bottom_first_stacking() {
    let svg = br#"<svg viewBox="0 0 10 10">
        <rect x="0" y="0" width="10" height="10" fill="black"/>
        <polygon points="2,2 8,2 8,8 2,8" fill="white"/>
    </svg>"#;
    let image = decode(svg).expect("two layers");
    assert_eq!(image.layers().len(), 2);
    assert_eq!(image.layers()[0].fill, Color::rgb(0, 0, 0));
    assert_eq!(image.layers()[1].fill, Color::rgb(255, 255, 255));
}

#[test]
fn rect_becomes_four_corners() {
    let svg = br##"<svg viewBox="0 0 20 20"><rect x="3" y="4" width="10" height="6" fill="#111"/></svg>"##;
    let image = decode(svg).expect("rect");
    assert_eq!(
        image.layers()[0].polygon,
        vec![(3, 4), (13, 4), (13, 10), (3, 10)]
    );
}

#[test]
fn rect_x_and_y_default_to_zero() {
    let svg = br##"<svg viewBox="0 0 20 20"><rect width="5" height="5" fill="#222"/></svg>"##;
    let image = decode(svg).expect("rect without x/y");
    assert_eq!(
        image.layers()[0].polygon,
        vec![(0, 0), (5, 0), (5, 5), (0, 5)]
    );
}

#[test]
fn path_straight_segments_build_a_ring() {
    let svg = br##"<svg viewBox="0 0 16 16"><path d="M 1 1 L 15 1 L 15 15 Z" fill="#0a0"/></svg>"##;
    let image = decode(svg).expect("path with straight segments");
    assert_eq!(image.layers()[0].polygon, vec![(1, 1), (15, 1), (15, 15)]);
}

#[test]
fn path_relative_and_shorthand_commands() {
    // M then implicit L pairs, a relative h, and a v close the box.
    let svg = br##"<svg viewBox="0 0 16 16"><path d="M2 2 h10 v10 h-10 Z" fill="#fff"/></svg>"##;
    let image = decode(svg).expect("relative path");
    assert_eq!(
        image.layers()[0].polygon,
        vec![(2, 2), (12, 2), (12, 12), (2, 12)]
    );
}

#[test]
fn three_digit_hex_expands() {
    let svg = br##"<svg viewBox="0 0 4 4"><polygon points="0,0 4,0 4,4" fill="#0f0"/></svg>"##;
    let image = decode(svg).expect("short hex");
    assert_eq!(image.layers()[0].fill, Color::rgb(0, 255, 0));
}

#[test]
fn fill_opacity_scales_alpha() {
    let svg = br##"<svg viewBox="0 0 4 4"><polygon points="0,0 4,0 4,4" fill="#ffffff" fill-opacity="0.5"/></svg>"##;
    let image = decode(svg).expect("opacity");
    assert_eq!(image.layers()[0].fill, Color::rgba(255, 255, 255, 128));
}

#[test]
fn hex_alpha_form_is_honoured() {
    let svg = br##"<svg viewBox="0 0 4 4"><polygon points="0,0 4,0 4,4" fill="#11223380"/></svg>"##;
    let image = decode(svg).expect("rgba hex");
    assert_eq!(image.layers()[0].fill, Color::rgba(0x11, 0x22, 0x33, 0x80));
}

#[test]
fn none_fill_contributes_no_layer() {
    let svg = br#"<svg viewBox="0 0 4 4"><polygon points="0,0 4,0 4,4" fill="none"/></svg>"#;
    let image = decode(svg).expect("none fill");
    assert!(image.layers().is_empty());
}

#[test]
fn zero_opacity_contributes_no_layer() {
    let svg = br##"<svg viewBox="0 0 4 4"><polygon points="0,0 4,0 4,4" fill="#fff" fill-opacity="0"/></svg>"##;
    let image = decode(svg).expect("transparent");
    assert!(image.layers().is_empty());
}

#[test]
fn missing_fill_defaults_to_black() {
    let svg = br#"<svg viewBox="0 0 4 4"><polygon points="0,0 4,0 4,4"/></svg>"#;
    let image = decode(svg).expect("default fill");
    assert_eq!(image.layers()[0].fill, Color::rgb(0, 0, 0));
}

#[test]
fn degenerate_polygon_is_dropped() {
    // A two-vertex ring covers no area and is silently skipped (§2.9).
    let svg = br##"<svg viewBox="0 0 4 4"><polygon points="0,0 4,4" fill="#fff"/></svg>"##;
    let image = decode(svg).expect("degenerate");
    assert!(image.layers().is_empty());
}

#[test]
fn comments_processing_instructions_and_doctype_are_skipped() {
    let svg = br##"<?xml version="1.0"?>
        <!DOCTYPE svg>
        <!-- a leading comment with <polygon> text inside it -->
        <svg viewBox="0 0 8 8">
          <polygon points="0,0 8,0 8,8" fill="#abc"/>
        </svg>"##;
    let image = decode(svg).expect("preamble skipped");
    assert_eq!(image.layers().len(), 1);
    assert_eq!(image.layers()[0].fill, Color::rgb(0xaa, 0xbb, 0xcc));
}

#[test]
fn width_height_fallback_when_no_view_box() {
    let svg =
        br##"<svg width="32px" height="32px"><rect width="32" height="32" fill="#000"/></svg>"##;
    let image = decode(svg).expect("width/height design");
    assert_eq!(image.design(), 32);
}

#[test]
fn hotspot_is_read_from_the_root() {
    let svg = br##"<svg viewBox="0 0 24 24" data-hotspot-x="3" data-hotspot-y="2"><polygon points="0,0 24,0 24,24" fill="#fff"/></svg>"##;
    let image = decode(svg).expect("hotspot");
    assert_eq!(image.hotspot(), Some((3, 2)));
}

#[test]
fn no_hotspot_is_none() {
    let svg = br##"<svg viewBox="0 0 24 24"><polygon points="0,0 24,0 24,24" fill="#fff"/></svg>"##;
    let image = decode(svg).expect("no hotspot");
    assert_eq!(image.hotspot(), None);
}

#[test]
fn half_a_hotspot_is_rejected() {
    let svg = br##"<svg viewBox="0 0 24 24" data-hotspot-x="3"><polygon points="0,0 24,0 24,24" fill="#fff"/></svg>"##;
    assert_eq!(decode(svg), Err(SvgError::InvalidNumber));
}

#[test]
fn no_root_is_rejected() {
    assert_eq!(decode(b"<group></group>"), Err(SvgError::MissingRoot));
}

#[test]
fn missing_view_box_is_rejected() {
    assert_eq!(
        decode(br#"<svg><polygon points="0,0 1,0 1,1"/></svg>"#),
        Err(SvgError::MissingViewBox)
    );
}

#[test]
fn non_square_view_box_is_rejected() {
    assert_eq!(
        decode(br#"<svg viewBox="0 0 24 12"></svg>"#),
        Err(SvgError::NonSquareViewBox)
    );
}

#[test]
fn nonzero_view_box_origin_is_rejected() {
    assert_eq!(
        decode(br#"<svg viewBox="1 0 24 24"></svg>"#),
        Err(SvgError::InvalidViewBox)
    );
}

#[test]
fn oversize_design_is_rejected() {
    assert_eq!(
        decode(br#"<svg viewBox="0 0 99999 99999"></svg>"#),
        Err(SvgError::InvalidViewBox)
    );
}

#[test]
fn fractional_coordinate_is_rejected() {
    assert_eq!(
        decode(br#"<svg viewBox="0 0 10 10"><polygon points="0.5,0 10,0 10,10"/></svg>"#),
        Err(SvgError::InvalidNumber)
    );
}

#[test]
fn odd_point_count_is_rejected() {
    assert_eq!(
        decode(br#"<svg viewBox="0 0 10 10"><polygon points="0,0 10,0 10"/></svg>"#),
        Err(SvgError::InvalidNumber)
    );
}

#[test]
fn unknown_colour_is_rejected() {
    assert_eq!(
        decode(br#"<svg viewBox="0 0 10 10"><polygon points="0,0 10,0 10,10" fill="chartreuse"/></svg>"#),
        Err(SvgError::InvalidColor)
    );
}

#[test]
fn curve_command_fails_closed() {
    assert_eq!(
        decode(br##"<svg viewBox="0 0 10 10"><path d="M0 0 C 1 1 2 2 3 3" fill="#fff"/></svg>"##),
        Err(SvgError::UnsupportedPath)
    );
}

#[test]
fn number_after_close_command_fails_closed() {
    // Regression: a stray operand after `Z` must not spin the path builder
    // (the implicit-repeat branch would never consume it). Found by fuzzing.
    assert_eq!(
        decode(br##"<svg viewBox="0 0 10 10"><path d="M0 0 L4 0 L4 4 Z 5" fill="#fff"/></svg>"##),
        Err(SvgError::UnsupportedPath)
    );
}

#[test]
fn second_subpath_fails_closed() {
    assert_eq!(
        decode(br##"<svg viewBox="0 0 10 10"><path d="M0 0 L4 0 L4 4 M5 5 L9 5 L9 9" fill="#fff"/></svg>"##),
        Err(SvgError::UnsupportedPath)
    );
}

#[test]
fn unterminated_tag_is_malformed() {
    assert_eq!(
        decode(br#"<svg viewBox="0 0 10 10"#),
        Err(SvgError::Malformed)
    );
}

#[test]
fn unterminated_comment_is_malformed() {
    assert_eq!(decode(br"<!-- never ends"), Err(SvgError::Malformed));
}

#[test]
fn non_utf8_is_rejected() {
    assert_eq!(decode(&[0xff, 0xfe, 0x00]), Err(SvgError::NotUtf8));
}

#[test]
fn empty_input_is_missing_root() {
    assert_eq!(decode(b""), Err(SvgError::MissingRoot));
}
