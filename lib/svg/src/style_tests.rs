//! Unit tests for the presentation-property cascade.

use alloc::format;
use alloc::string::ToString;

use tairix_raster::{Color, FillRule};

use crate::error::SvgError;
use crate::geom::{LineCap, LineJoin};
use crate::xml;

use super::{PaintSlot, PaintSpec, Style};

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
    parent.apply(child, VIEWPORT, &[])
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
    assert_eq!(style.fill, PaintSpec::Color(Color::rgb(0, 0, 0)));
    assert!((style.fill_opacity - 1.0).abs() < 1e-9);
    assert_eq!(style.stroke, PaintSpec::None);
    assert_eq!(style.fill_rule, FillRule::NonZero);
}

/// The two opacities are separate properties; the product is the drawing's
/// business, because a group opacity composites a subtree rather than a
/// paint.
#[test]
fn a_paint_opacity_and_a_group_opacity_are_held_apart() {
    let style = styled(r#"<rect fill="black" fill-opacity="0.5" opacity="0.25"/>"#);
    assert!((style.fill_opacity - 0.5).abs() < 1e-9);
    assert!((style.opacity - 0.25).abs() < 1e-9);
}

/// CSS resolves `currentColor` against the element's *final* `color`, which
/// an attribute later in the same tag may still change — so it cannot be
/// resolved where it is written.
#[test]
fn current_color_resolves_against_the_final_color_property() {
    let style = styled(r##"<rect fill="currentColor" color="#00ff00"/>"##);
    assert_eq!(style.fill, PaintSpec::Current);
    assert_eq!(style.color, Color::rgb(0, 255, 0));

    let reordered = styled(r##"<rect color="#00ff00" fill="currentColor"/>"##);
    assert_eq!(reordered.fill, PaintSpec::Current);
    assert_eq!(reordered.color, Color::rgb(0, 255, 0));
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

// --- paint order, as a three-way permutation -------------------------------

/// The initial order, which `normal` also spells outright.
#[test]
fn the_initial_paint_order_is_fill_then_stroke_then_markers() {
    use PaintSlot::{Fill, Markers, Stroke};
    assert_eq!(
        styled("<rect/>").paint_order.slots(),
        [Fill, Stroke, Markers]
    );
    assert_eq!(
        styled(r#"<rect paint-order="normal"/>"#)
            .paint_order
            .slots(),
        [Fill, Stroke, Markers]
    );
}

/// Whichever slots are named come first in the order they are written, and
/// whichever are left follow in the initial order.
#[test]
fn the_named_slots_come_first_and_the_rest_follow_initially() {
    use PaintSlot::{Fill, Markers, Stroke};
    for (value, expected) in [
        ("markers fill stroke", [Markers, Fill, Stroke]),
        ("stroke", [Stroke, Fill, Markers]),
        ("markers", [Markers, Fill, Stroke]),
        ("stroke markers", [Stroke, Markers, Fill]),
        ("fill stroke markers", [Fill, Stroke, Markers]),
        ("  stroke   fill  ", [Stroke, Fill, Markers]),
    ] {
        let style = styled(&format!(r#"<rect paint-order="{value}"/>"#));
        assert_eq!(style.paint_order.slots(), expected, "{value}");
    }
}

/// An invalid value is a dropped declaration, so the element keeps what it
/// inherited rather than snapping back to the initial order — and the
/// document is not refused over it, which is what CSS does.
#[test]
fn an_invalid_paint_order_is_dropped_rather_than_reset() {
    use PaintSlot::{Fill, Markers, Stroke};
    let parent = styled(r#"<g paint-order="stroke"/>"#);
    for value in [
        "",
        "sideways",
        "fill fill",
        "stroke normal",
        "markers markers",
    ] {
        let style = resolve(&parent, &format!(r#"<rect paint-order="{value}"/>"#))
            .expect("a dropped declaration is not an error");
        assert_eq!(
            style.paint_order.slots(),
            [Stroke, Fill, Markers],
            "{value}"
        );
    }
}

/// Paint order inherits, so a container can set it for a whole subtree.
#[test]
fn paint_order_inherits() {
    use PaintSlot::{Fill, Markers, Stroke};
    let parent = styled(r#"<g paint-order="markers"/>"#);
    assert_eq!(
        parent.inherit().paint_order.slots(),
        [Markers, Fill, Stroke]
    );
}

// --- the marker properties -------------------------------------------------

#[test]
fn each_marker_property_takes_the_fragment_it_names() {
    let style =
        styled(r#"<path marker-start="url(#a)" marker-mid="url(#b)" marker-end="url(#c)"/>"#);
    assert_eq!(style.marker_start.as_deref(), Some("a"));
    assert_eq!(style.marker_mid.as_deref(), Some("b"));
    assert_eq!(style.marker_end.as_deref(), Some("c"));
}

/// The marker properties inherit, unlike the compositing ones: a marker is
/// drawn at every vertex of every descendant shape, not once around the
/// subtree.
#[test]
fn the_marker_properties_inherit() {
    let parent = styled(r#"<g marker-mid="url(#dot)"/>"#);
    assert_eq!(parent.inherit().marker_mid.as_deref(), Some("dot"));
    // Which is exactly what a clip does not do.
    assert_eq!(
        styled(r#"<g clip-path="url(#c)"/>"#).inherit().clip_path,
        None
    );
}

/// The `marker` shorthand sets all three at once.
#[test]
fn the_marker_shorthand_sets_all_three() {
    let style = styled(r#"<path style="marker:url(#dot)"/>"#);
    assert_eq!(style.marker_start.as_deref(), Some("dot"));
    assert_eq!(style.marker_mid.as_deref(), Some("dot"));
    assert_eq!(style.marker_end.as_deref(), Some("dot"));
    assert_eq!(styled(r#"<path style="marker:none"/>"#).marker_start, None);
}

/// SVG publishes the shorthand to CSS alone — there is no `marker`
/// presentation attribute — so an attribute of that name sets nothing, and
/// honouring it would draw markers no other renderer does.
#[test]
fn the_marker_shorthand_is_not_a_presentation_attribute() {
    let style = styled(r#"<path marker="url(#dot)"/>"#);
    assert_eq!(style.marker_start, None);
    assert_eq!(style.marker_mid, None);
    assert_eq!(style.marker_end, None);
}

/// A marker value that is neither `none` nor a local reference names a
/// composite this decoder cannot build, and is refused like a clip's.
#[test]
fn a_malformed_marker_reference_is_refused() {
    assert_eq!(
        resolve(&Style::default(), r#"<path marker-start="sideways"/>"#),
        Err(SvgError::InvalidReference)
    );
}
