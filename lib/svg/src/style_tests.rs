//! Unit tests for the presentation-property cascade.

use alloc::format;
use alloc::string::ToString;

use tairix_raster::{Color, FillRule};

use crate::error::SvgError;
use crate::geom::{LineCap, LineJoin};
use crate::xml;

use super::{PaintSpec, Style};

/// The viewport length a percentage resolves against in these tests.
const VIEWPORT: f64 = 100.0;

/// The style one element resolves to, starting from SVG's initial values.
#[track_caller]
fn styled(tag: &str) -> Style {
    resolve(&Style::default(), tag).expect("a style")
}

/// The style one element resolves to, inheriting from `parent`.
#[track_caller]
fn resolve(parent: &Style, tag: &str) -> Result<Style, SvgError> {
    let document = format!("<svg>{tag}</svg>");
    let root = xml::parse(&document).expect("a document");
    let child = root.children.first().expect("a child element");
    parent.apply(child, VIEWPORT)
}

// --- where a property comes from ------------------------------------------

#[test]
fn a_presentation_attribute_sets_its_property() {
    let style = styled(r##"<rect fill="#ff0000"/>"##);
    assert_eq!(style.fill, PaintSpec::Color(Color::rgb(255, 0, 0)));
}

/// The `style` attribute wins over the presentation attribute, which is what
/// CSS specificity says.
#[test]
fn an_inline_declaration_beats_the_attribute() {
    let style = styled(r##"<rect fill="#ff0000" style="fill:#00ff00"/>"##);
    assert_eq!(style.fill, PaintSpec::Color(Color::rgb(0, 255, 0)));
}

#[test]
fn an_inline_declaration_list_sets_each_property() {
    let style = styled(r#"<rect style="fill:none; stroke:black ;stroke-width:3"/>"#);
    assert_eq!(style.fill, PaintSpec::None);
    assert_eq!(style.stroke, PaintSpec::Color(Color::rgb(0, 0, 0)));
    assert!((style.stroke_style.width - 3.0).abs() < 1e-9);
}

/// A document is full of editor metadata and text properties that have no
/// bearing on the shapes drawn; refusing them would reject nearly every real
/// asset.
#[test]
fn an_unknown_property_is_ignored_rather_than_refused() {
    let style = styled(r#"<rect font-family="Serif" inkscape:label="x" style="font-size:12"/>"#);
    assert_eq!(style, Style::default());
}

/// A property it *does* understand but cannot read is a different matter: a
/// malformed colour or width fails closed.
#[test]
fn a_malformed_known_property_is_refused() {
    assert_eq!(
        resolve(&Style::default(), r#"<rect fill="not-a-colour"/>"#),
        Err(SvgError::InvalidColor)
    );
    assert_eq!(
        resolve(&Style::default(), r#"<rect stroke-width="wide"/>"#),
        Err(SvgError::InvalidNumber)
    );
    assert_eq!(
        resolve(&Style::default(), r#"<rect fill-rule="sideways"/>"#),
        Err(SvgError::InvalidNumber)
    );
}

// --- inheritance ----------------------------------------------------------

#[test]
fn paint_and_stroke_properties_inherit() {
    let parent = styled(r##"<g fill="#123456" stroke="#654321" stroke-width="4"/>"##);
    let child = parent.inherit();
    assert_eq!(child.fill, PaintSpec::Color(Color::rgb(0x12, 0x34, 0x56)));
    assert_eq!(child.stroke, PaintSpec::Color(Color::rgb(0x65, 0x43, 0x21)));
    assert!((child.stroke_style.width - 4.0).abs() < 1e-9);
}

/// Group opacity composites the subtree as a unit, so it applies where it is
/// written and is not handed down to be applied a second time.
#[test]
fn group_opacity_does_not_inherit() {
    let parent = styled(r#"<g opacity="0.5"/>"#);
    assert!((parent.opacity - 0.5).abs() < 1e-9);
    assert!((parent.inherit().opacity - 1.0).abs() < 1e-9);
}

#[test]
fn display_none_does_not_inherit_but_visibility_does() {
    let hidden = styled(r#"<g display="none" visibility="hidden"/>"#);
    assert!(!hidden.display);
    assert!(!hidden.visible);
    let child = hidden.inherit();
    assert!(child.display);
    assert!(!child.visible);
}

// --- paints ---------------------------------------------------------------

#[test]
fn the_initial_fill_is_opaque_black_and_the_initial_stroke_is_none() {
    let style = Style::default();
    assert_eq!(style.fill_color(), Some(Color::rgb(0, 0, 0)));
    assert_eq!(style.stroke_color(), None);
    assert_eq!(style.fill_rule, FillRule::NonZero);
}

#[test]
fn fill_opacity_and_group_opacity_multiply() {
    let style = styled(r#"<rect fill="black" fill-opacity="0.5" opacity="0.5"/>"#);
    let color = style.fill_color().expect("a quarter-opaque black");
    assert_eq!(color.a, 64);
}

#[test]
fn a_fully_transparent_paint_is_nothing_at_all() {
    let style = styled(r#"<rect fill="black" fill-opacity="0"/>"#);
    assert_eq!(style.fill_color(), None);
}

/// CSS resolves `currentColor` against the element's *final* `color`, which
/// an attribute later in the same tag may still change — so it cannot be
/// resolved where it is written.
#[test]
fn current_color_resolves_against_the_final_color_property() {
    let style = styled(r##"<rect fill="currentColor" color="#00ff00"/>"##);
    assert_eq!(style.fill, PaintSpec::Current);
    assert_eq!(style.fill_color(), Some(Color::rgb(0, 255, 0)));

    let reordered = styled(r##"<rect color="#00ff00" fill="currentColor"/>"##);
    assert_eq!(reordered.fill_color(), Some(Color::rgb(0, 255, 0)));
}

#[test]
fn a_paint_server_reference_keeps_its_name_and_fallback() {
    let style = styled(r#"<rect fill="url(#grad)"/>"#);
    assert_eq!(style.fill, PaintSpec::Reference("grad".to_string(), None));

    let with_fallback = styled(r#"<rect fill="url(#grad) #ff0000"/>"#);
    assert_eq!(
        with_fallback.fill,
        PaintSpec::Reference("grad".to_string(), Some(Color::rgb(255, 0, 0)))
    );
}

#[test]
fn a_malformed_paint_reference_is_refused() {
    assert_eq!(
        resolve(&Style::default(), r#"<rect fill="url(grad)"/>"#),
        Err(SvgError::InvalidColor)
    );
    assert_eq!(
        resolve(&Style::default(), r#"<rect fill="url(#grad"/>"#),
        Err(SvgError::InvalidColor)
    );
}

// --- stroke geometry ------------------------------------------------------

#[test]
fn the_stroke_keywords_all_parse() {
    let style =
        styled(r#"<rect stroke-linecap="round" stroke-linejoin="bevel" stroke-miterlimit="8"/>"#);
    assert_eq!(style.stroke_style.cap, LineCap::Round);
    assert_eq!(style.stroke_style.join, LineJoin::Bevel);
    assert!((style.stroke_style.miter_limit - 8.0).abs() < 1e-9);
}

/// SVG floors the miter limit at one, below which the ratio has no meaning.
#[test]
fn the_miter_limit_is_floored_at_one() {
    let style = styled(r#"<rect stroke-miterlimit="0.1"/>"#);
    assert!((style.stroke_style.miter_limit - 1.0).abs() < 1e-9);
}

#[test]
fn a_stroke_width_may_be_a_percentage_or_carry_a_unit() {
    assert!((styled(r#"<rect stroke-width="10%"/>"#).stroke_style.width - 10.0).abs() < 1e-9);
    assert!((styled(r#"<rect stroke-width="1in"/>"#).stroke_style.width - 96.0).abs() < 1e-9);
}

#[test]
fn a_dash_array_reads_its_lengths() {
    let style = styled(r#"<rect stroke-dasharray="4 2, 6" stroke-dashoffset="3"/>"#);
    assert_eq!(style.stroke_style.dashes, alloc::vec![4.0, 2.0, 6.0]);
    assert!((style.stroke_style.dash_offset - 3.0).abs() < 1e-9);
}

/// `none`, and any pattern that could never draw, mean a solid stroke.
#[test]
fn a_dash_array_that_cannot_draw_is_solid() {
    assert!(styled(r#"<rect stroke-dasharray="none"/>"#)
        .stroke_style
        .dashes
        .is_empty());
    assert!(styled(r#"<rect stroke-dasharray="0 0"/>"#)
        .stroke_style
        .dashes
        .is_empty());
    assert!(styled(r#"<rect stroke-dasharray="-4 2"/>"#)
        .stroke_style
        .dashes
        .is_empty());
}

/// A dash pattern is a handful of lengths in every real asset; an unbounded
/// one would make the stroker walk it forever.
#[test]
fn an_unbounded_dash_array_is_refused() {
    let many = "1 ".repeat(200);
    let tag = format!(r#"<rect stroke-dasharray="{many}"/>"#);
    assert_eq!(resolve(&Style::default(), &tag), Err(SvgError::TooComplex));
}

#[test]
fn the_fill_rule_keywords_both_parse() {
    assert_eq!(
        styled(r#"<rect fill-rule="evenodd"/>"#).fill_rule,
        FillRule::EvenOdd
    );
    assert_eq!(
        styled(r#"<rect fill-rule="nonzero"/>"#).fill_rule,
        FillRule::NonZero
    );
}
