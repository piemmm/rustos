//! Unit tests for the `<text>` layout: the positioning rules of SVG 1.1,
//! the white-space model, and the bounds a hostile document is held to.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use super::{
    collect, lay_out, Collected, TextBudget, TextCascade, MAX_PROVIDER_REQUESTS, MAX_TEXT_LENGTH,
    MAX_TSPAN_DEPTH,
};
use crate::error::SvgError;
use crate::font::{
    FaceId, FaceMetrics, FaceRequest, FontProvider, FontUnavailable, GlyphOutline, OutlineContour,
    OutlineSegment,
};
use crate::style::Style;
use crate::xml::{self, Element};

/// The em every test face is drawn on.
const EM: f64 = 1000.0;

/// The advance every test glyph but the space has.
const ADVANCE: f64 = 500.0;

/// A provider whose every glyph is one square of half an em, so an advance
/// is a round number and a layout can be checked by arithmetic.
struct Squares {
    /// Families this provider refuses, so the family ladder can be driven.
    refuse: Vec<String>,
    /// Every face request it was asked, in order.
    asked: Vec<(String, u16)>,
    /// How many outline calls it answered.
    calls: usize,
    /// Answer one glyph fewer than asked for, to drive the refusal.
    short: bool,
}

impl Squares {
    fn new() -> Self {
        Self {
            refuse: Vec::new(),
            asked: Vec::new(),
            calls: 0,
            short: false,
        }
    }

    fn refusing(names: &[&str]) -> Self {
        Self {
            refuse: names.iter().map(|name| String::from(*name)).collect(),
            ..Self::new()
        }
    }
}

impl FontProvider for Squares {
    fn select(&mut self, req: &FaceRequest<'_>) -> Result<FaceMetrics, FontUnavailable> {
        self.asked.push((String::from(req.family), req.weight));
        if self.refuse.iter().any(|name| name == req.family) {
            return Err(FontUnavailable);
        }
        Ok(FaceMetrics {
            id: FaceId::new(1),
            units_per_em: EM,
            ascent: 800.0,
            descent: 200.0,
            line_gap: 0.0,
        })
    }

    fn outlines(
        &mut self,
        _face: FaceId,
        run: &[char],
        out: &mut Vec<GlyphOutline>,
    ) -> Result<(), FontUnavailable> {
        self.calls += 1;
        let take = if self.short {
            run.len().saturating_sub(1)
        } else {
            run.len()
        };
        for scalar in run.iter().take(take) {
            out.push(GlyphOutline {
                units_per_em: EM,
                advance: ADVANCE,
                synthetic_bold: 0.0,
                synthetic_shear: 0.0,
                contours: if *scalar == ' ' {
                    Vec::new()
                } else {
                    vec![OutlineContour {
                        start: (0.0, 0.0),
                        segments: vec![
                            OutlineSegment::Line { to: (ADVANCE, 0.0) },
                            OutlineSegment::Line { to: (ADVANCE, EM) },
                            OutlineSegment::Line { to: (0.0, EM) },
                            OutlineSegment::Line { to: (0.0, 0.0) },
                        ],
                    }]
                },
            });
        }
        Ok(())
    }
}

/// A cascade that applies each element's own presentation attributes and
/// nothing else — enough to drive the positioning rules without the
/// document's stylesheet.
struct Attributes {
    depth: usize,
}

impl TextCascade<'_> for Attributes {
    fn enter(&mut self, element: &Element<'_>, inherited: &Style) -> Result<Style, SvgError> {
        self.depth += 1;
        inherited.apply(element, 100.0, &[])
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }
}

/// Assert two user-space lengths agree to well within a design-grid step.
#[track_caller]
fn near(got: f64, want: f64) {
    assert!((got - want).abs() < 1e-9, "expected {want}, got {got}");
}

/// Assert a run of pen positions.
#[track_caller]
fn at(got: &[(f64, f64)], want: &[(f64, f64)]) {
    assert_eq!(got.len(), want.len(), "{got:?} is not {want:?}");
    for (index, (place, expected)) in got.iter().zip(want).enumerate() {
        assert!(
            (place.0 - expected.0).abs() < 1e-9 && (place.1 - expected.1).abs() < 1e-9,
            "glyph {index}: expected {expected:?}, got {place:?}"
        );
    }
}

/// The pen positions a layout resolved, and the provider that answered it.
type LaidOut = Option<(Vec<(f64, f64)>, Squares)>;

/// Parse `document`, collect its root `<text>`, and lay it out.
#[track_caller]
fn laid(document: &str) -> Result<LaidOut, SvgError> {
    let root = xml::parse(document).expect("a document");
    let mut cascade = Attributes { depth: 0 };
    let style = Style::default()
        .apply(&root, 100.0, &[])
        .expect("a root style");
    let collected = collect(&root, &style, (100.0, 100.0), &mut cascade)?;
    let mut provider = Squares::new();
    let mut budget = TextBudget::default();
    let laid = lay_out(collected, &mut provider, &mut budget)?;
    Ok(laid.map(|laid| {
        let origins = laid
            .glyphs
            .iter()
            .map(|glyph| (glyph.to_user.e, glyph.to_user.f))
            .collect();
        (origins, provider)
    }))
}

/// The pen positions `document`'s text resolves to.
#[track_caller]
fn origins(document: &str) -> Vec<(f64, f64)> {
    laid(document).expect("a layout").expect("some text").0
}

/// Collect without laying out, for the tests that only exercise the walk.
#[track_caller]
fn collected(document: &str) -> Result<Collected, SvgError> {
    let root = xml::parse(document).expect("a document");
    let mut cascade = Attributes { depth: 0 };
    let style = Style::default()
        .apply(&root, 100.0, &[])
        .expect("a root style");
    collect(&root, &style, (100.0, 100.0), &mut cascade)
}

#[test]
fn the_pen_advances_by_each_glyphs_own_advance() {
    // Half an em at the initial 16px size is 8 user units.
    let step = 16.0 * ADVANCE / EM;
    at(
        &origins(r#"<text x="10" y="20">abc</text>"#),
        &[(10.0, 20.0), (10.0 + step, 20.0), (10.0 + 2.0 * step, 20.0)],
    );
}

#[test]
fn a_position_list_addresses_each_character_in_turn() {
    at(
        &origins(r#"<text x="1 5 9" y="2">abc</text>"#),
        &[(1.0, 2.0), (5.0, 2.0), (9.0, 2.0)],
    );
}

#[test]
fn a_shift_list_is_relative_to_where_the_pen_already_is() {
    let step = 16.0 * ADVANCE / EM;
    at(
        &origins(r#"<text x="0" y="0" dx="0 3" dy="0 4">ab</text>"#),
        &[(0.0, 0.0), (step + 3.0, 4.0)],
    );
}

#[test]
fn a_tspans_characters_are_addressable_by_its_ancestor_too() {
    // The `<text>` states three x values; the second and third are taken by
    // the characters the `<tspan>` contributed.
    at(
        &origins(r#"<text x="1 5 9" y="0">a<tspan>bc</tspan></text>"#),
        &[(1.0, 0.0), (5.0, 0.0), (9.0, 0.0)],
    );
}

#[test]
fn a_tspans_own_position_wins_over_its_ancestors() {
    at(
        &origins(r#"<text x="1 5" y="0">a<tspan x="40">b</tspan></text>"#),
        &[(1.0, 0.0), (40.0, 0.0)],
    );
}

#[test]
fn white_space_collapses_across_an_element_boundary() {
    // `a` then one space then `b`: the space before `b` and the one after
    // `a` are one space, and the leading indentation is stripped.
    let step = 16.0 * ADVANCE / EM;
    let placed = origins("<text x=\"0\" y=\"0\">  a <tspan> b</tspan></text>");
    assert_eq!(placed.len(), 3, "expected `a`, one space, `b`");
    at(&placed, &[(0.0, 0.0), (step, 0.0), (2.0 * step, 0.0)]);
}

#[test]
fn a_newline_is_dropped_when_space_is_collapsed_and_kept_when_preserved() {
    let collapsed = collected("<text>a\nb</text>").expect("a walk");
    assert_eq!(collapsed.chars.len(), 2, "the newline should have gone");

    let preserved = collected("<text xml:space=\"preserve\">a\nb</text>").expect("a walk");
    assert_eq!(preserved.chars.len(), 3);
    assert_eq!(preserved.chars[1].scalar, ' ', "a newline becomes a space");
}

#[test]
fn preserved_space_keeps_its_leading_and_trailing_runs() {
    let preserved = collected("<text xml:space=\"preserve\">  a  </text>").expect("a walk");
    assert_eq!(preserved.chars.len(), 5);
}

#[test]
fn a_tab_is_a_space_under_either_treatment() {
    for document in [
        "<text>a\tb</text>",
        "<text xml:space=\"preserve\">a\tb</text>",
    ] {
        let walked = collected(document).expect("a walk");
        assert_eq!(walked.chars[1].scalar, ' ', "{document}");
    }
}

#[test]
fn text_anchor_shifts_the_whole_chunk() {
    let step = 16.0 * ADVANCE / EM;
    let middle = origins(r#"<text x="100" y="0" text-anchor="middle">ab</text>"#);
    near(middle[0].0, 100.0 - step);
    near(middle[1].0, 100.0);

    let end = origins(r#"<text x="100" y="0" text-anchor="end">ab</text>"#);
    near(end[0].0, 100.0 - 2.0 * step);
}

#[test]
fn each_absolute_position_begins_a_new_anchored_chunk() {
    let step = 16.0 * ADVANCE / EM;
    // Two chunks of one glyph each: every glyph is anchored on its own x.
    let placed = origins(r#"<text x="10 50" y="0" text-anchor="end">ab</text>"#);
    near(placed[0].0, 10.0 - step);
    near(placed[1].0, 50.0 - step);
}

#[test]
fn letter_and_word_spacing_widen_the_advance() {
    let step = 16.0 * ADVANCE / EM;
    let spaced = origins(r#"<text x="0" y="0" letter-spacing="2">ab</text>"#);
    near(spaced[1].0, step + 2.0);

    let worded = origins(r#"<text x="0" y="0" xml:space="preserve" word-spacing="5">a b</text>"#);
    // `a`, then the space (one advance on), then `b` after the space's own
    // advance plus the word spacing.
    near(worded[2].0, 2.0 * step + 5.0);
}

#[test]
fn text_length_spreads_the_difference_across_the_gaps() {
    // Three glyphs naturally span 3 steps; `textLength` widens the two gaps.
    let step = 16.0 * ADVANCE / EM;
    let natural = 3.0 * step;
    let target = natural + 20.0;
    let document = alloc::format!(r#"<text x="0" y="0" textLength="{target}">abc</text>"#);
    let placed = origins(&document);
    near(placed[1].0, step + 10.0);
    near(placed[2].0, 2.0 * step + 20.0);
}

#[test]
fn length_adjust_spacing_and_glyphs_scales_the_letterforms_too() {
    let root = xml::parse(
        r#"<text x="0" y="0" textLength="48" lengthAdjust="spacingAndGlyphs">ab</text>"#,
    )
    .expect("a document");
    let mut cascade = Attributes { depth: 0 };
    let style = Style::default()
        .apply(&root, 100.0, &[])
        .expect("a root style");
    let walked = collect(&root, &style, (100.0, 100.0), &mut cascade).expect("a walk");
    let mut provider = Squares::new();
    let mut budget = TextBudget::default();
    let laid = lay_out(walked, &mut provider, &mut budget)
        .expect("a layout")
        .expect("some text");
    // Two glyphs of 8 units naturally span 16; stretched to 48 the glyphs
    // are three times as wide, which shows in the transform's own x scale.
    assert!((laid.glyphs[0].to_user.a - 3.0 * 16.0 / EM).abs() < 1e-9);
}

#[test]
fn an_unknown_length_adjust_refuses_the_document() {
    assert_eq!(
        collected(r#"<text textLength="10" lengthAdjust="sideways">a</text>"#).err(),
        Some(SvgError::InvalidNumber)
    );
}

#[test]
fn rotate_turns_each_glyph_and_its_last_value_persists() {
    let root = xml::parse(r#"<text x="0" y="0" rotate="90">ab</text>"#).expect("a document");
    let mut cascade = Attributes { depth: 0 };
    let style = Style::default()
        .apply(&root, 100.0, &[])
        .expect("a root style");
    let walked = collect(&root, &style, (100.0, 100.0), &mut cascade).expect("a walk");
    let mut provider = Squares::new();
    let mut budget = TextBudget::default();
    let laid = lay_out(walked, &mut provider, &mut budget)
        .expect("a layout")
        .expect("some text");
    // A quarter turn maps the x axis onto y, so the transform's own `a`
    // coefficient vanishes for *both* glyphs — the last rotate value
    // persists past the end of the list.
    for glyph in &laid.glyphs {
        assert!(glyph.to_user.a.abs() < 1e-9, "{:?}", glyph.to_user);
    }
}

#[test]
fn the_family_ladder_falls_through_to_the_generic_and_then_closed() {
    let root =
        xml::parse(r#"<text font-family="Nothing, AlsoNothing">a</text>"#).expect("a document");
    let mut cascade = Attributes { depth: 0 };
    let style = Style::default()
        .apply(&root, 100.0, &[])
        .expect("a root style");
    let walked = collect(&root, &style, (100.0, 100.0), &mut cascade).expect("a walk");
    let mut provider = Squares::refusing(&["Nothing", "AlsoNothing"]);
    let mut budget = TextBudget::default();
    lay_out(walked, &mut provider, &mut budget)
        .expect("a layout")
        .expect("some text");
    assert_eq!(
        provider
            .asked
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        vec!["Nothing", "AlsoNothing", "sans-serif"],
        "the ladder must try each name then the generic"
    );
}

#[test]
fn a_family_nothing_can_furnish_fails_the_document_closed() {
    let root = xml::parse(r#"<text font-family="Nothing">a</text>"#).expect("a document");
    let mut cascade = Attributes { depth: 0 };
    let style = Style::default()
        .apply(&root, 100.0, &[])
        .expect("a root style");
    let walked = collect(&root, &style, (100.0, 100.0), &mut cascade).expect("a walk");
    let mut provider = Squares::refusing(&["Nothing", "sans-serif"]);
    let mut budget = TextBudget::default();
    assert_eq!(
        lay_out(walked, &mut provider, &mut budget).err(),
        Some(SvgError::FontUnavailable),
        "absent lettering is a wrong picture, not a missing decoration"
    );
}

#[test]
fn one_face_is_resolved_for_spans_that_ask_for_the_same_one() {
    let root = xml::parse(r#"<text>a<tspan fill="red">b</tspan><tspan>c</tspan></text>"#)
        .expect("a document");
    let mut cascade = Attributes { depth: 0 };
    let style = Style::default()
        .apply(&root, 100.0, &[])
        .expect("a root style");
    let walked = collect(&root, &style, (100.0, 100.0), &mut cascade).expect("a walk");
    let mut provider = Squares::new();
    let mut budget = TextBudget::default();
    lay_out(walked, &mut provider, &mut budget).expect("a layout");
    assert_eq!(
        provider.asked.len(),
        1,
        "three spans of one face must cost one resolution, not three"
    );
}

#[test]
fn a_provider_answering_a_different_run_is_refused() {
    let root = xml::parse("<text>abc</text>").expect("a document");
    let mut cascade = Attributes { depth: 0 };
    let style = Style::default()
        .apply(&root, 100.0, &[])
        .expect("a root style");
    let walked = collect(&root, &style, (100.0, 100.0), &mut cascade).expect("a walk");
    let mut provider = Squares::new();
    provider.short = true;
    let mut budget = TextBudget::default();
    assert_eq!(
        lay_out(walked, &mut provider, &mut budget).err(),
        Some(SvgError::FontUnavailable)
    );
}

#[test]
fn an_empty_text_lays_nothing_out_rather_than_failing() {
    assert!(laid("<text></text>").expect("a layout").is_none());
    assert!(laid("<text>   </text>").expect("a layout").is_none());
}

#[test]
fn tspan_nesting_past_the_bound_is_refused() {
    let mut document = String::from("<text>");
    for _ in 0..=MAX_TSPAN_DEPTH {
        document.push_str("<tspan>");
    }
    document.push('a');
    for _ in 0..=MAX_TSPAN_DEPTH {
        document.push_str("</tspan>");
    }
    document.push_str("</text>");
    assert_eq!(collected(&document).err(), Some(SvgError::TooComplex));
}

#[test]
fn text_longer_than_the_bound_is_refused() {
    let mut document = String::from("<text xml:space=\"preserve\">");
    document.extend(core::iter::repeat_n('a', MAX_TEXT_LENGTH + 1));
    document.push_str("</text>");
    assert_eq!(collected(&document).err(), Some(SvgError::TooComplex));
}

#[test]
fn a_document_cannot_spend_more_provider_calls_than_the_bound() {
    let mut budget = TextBudget::default();
    let mut provider = Squares::refusing(&[]);
    for _ in 0..MAX_PROVIDER_REQUESTS {
        let walked = collected("<text>a</text>").expect("a walk");
        // Each layout costs one `select` and one `outlines`, so the budget
        // runs out part way through and the document is refused rather than
        // going on calling.
        if lay_out(walked, &mut provider, &mut budget).is_err() {
            return;
        }
    }
    panic!("the provider-request bound never bit");
}

#[test]
fn a_glyph_run_is_split_at_the_protocol_run_length() {
    let mut document = String::from("<text xml:space=\"preserve\">");
    document.extend(core::iter::repeat_n('a', 70));
    document.push_str("</text>");
    let walked = collected(&document).expect("a walk");
    let mut provider = Squares::new();
    let mut budget = TextBudget::default();
    lay_out(walked, &mut provider, &mut budget).expect("a layout");
    assert_eq!(
        provider.calls, 3,
        "70 characters at 32 per request is three round trips"
    );
}
