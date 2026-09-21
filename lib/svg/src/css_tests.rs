//! Unit tests for the stylesheet: what the selector subset matches, how the
//! cascade orders what it finds, and what it drops rather than refuses.

use core::fmt::Write as _;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::{declarations, Declaration, Stylesheet};
use crate::error::SvgError;
use crate::xml::{self, Element};

/// The declarations the element reached by `path` matches, in cascade order.
///
/// Each step of `path` names a child by its element name or its `id`, so a
/// test addresses the element it means without a second tree walker.
#[track_caller]
fn matched(document: &str, path: &[&str]) -> Vec<(String, String)> {
    let root = xml::parse(document).expect("a document");
    let sheets = crate::css::sheet_texts(&root);
    let sheet = Stylesheet::collect(&sheets).expect("a sheet");
    let mut chain: Vec<&Element<'_>> = alloc::vec![&root];
    for step in path {
        let parent = *chain.last().expect("a parent");
        let child = parent
            .children()
            .find(|child| child.attr("id") == Some(*step) || child.name == *step)
            .unwrap_or_else(|| panic!("no child {step}"));
        chain.push(child);
    }
    let mut out = Vec::new();
    sheet.cascade(&chain, &mut out);
    out.into_iter()
        .map(|rule| (rule.name.to_string(), rule.value.to_string()))
        .collect()
}

/// The value the cascade leaves for `property`, which is the last it yields.
#[track_caller]
fn resolved(document: &str, path: &[&str], property: &str) -> Option<String> {
    matched(document, path)
        .into_iter()
        .rfind(|(name, _)| name == property)
        .map(|(_, value)| value)
}

// --- the declaration splitter ---------------------------------------------

#[test]
fn a_block_splits_into_its_declarations() {
    let parsed: Vec<Declaration<'_>> = declarations("fill:red;stroke : blue ;").collect();
    assert_eq!(
        parsed,
        [
            Declaration {
                name: "fill",
                value: "red",
                important: false,
            },
            Declaration {
                name: "stroke",
                value: "blue",
                important: false,
            },
        ]
    );
}

/// A separator inside brackets is text: a value that carries one must not be
/// cut in half.
#[test]
fn a_separator_inside_brackets_is_not_one() {
    let parsed: Vec<Declaration<'_>> = declarations("fill:url(a;b:c);stroke:red").collect();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].value, "url(a;b:c)");
    assert_eq!(parsed[1].value, "red");
}

#[test]
fn important_is_recognised_whatever_its_spacing_or_case() {
    for spelling in ["red !important", "red!IMPORTANT", "red ! Important"] {
        let block = format!("fill:{spelling}");
        let parsed: Vec<Declaration<'_>> = declarations(&block).collect();
        assert_eq!(parsed.len(), 1, "{spelling}");
        assert_eq!(parsed[0].value, "red", "{spelling}");
        assert!(parsed[0].important, "{spelling}");
    }
}

#[test]
fn a_declaration_with_no_colon_or_no_name_is_dropped() {
    let parsed: Vec<Declaration<'_>> = declarations("fill;:red; :blue ;stroke:red").collect();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].name, "stroke");
}

// --- what the selector subset matches -------------------------------------

#[test]
fn a_class_selector_matches_one_of_several_classes() {
    let document = r#"<svg><style>.b{fill:red}</style><rect class="a b c"/></svg>"#;
    assert_eq!(
        resolved(document, &["rect"], "fill").as_deref(),
        Some("red")
    );
}

#[test]
fn a_type_and_an_id_selector_match() {
    let document = r#"<svg><style>rect{fill:red}#r{stroke:blue}</style><rect id="r"/></svg>"#;
    assert_eq!(resolved(document, &["r"], "fill").as_deref(), Some("red"));
    assert_eq!(
        resolved(document, &["r"], "stroke").as_deref(),
        Some("blue")
    );
}

#[test]
fn the_universal_selector_matches_anything() {
    let document = r"<svg><style>*{fill:red}</style><rect/></svg>";
    assert_eq!(
        resolved(document, &["rect"], "fill").as_deref(),
        Some("red")
    );
}

#[test]
fn a_compound_selector_needs_every_condition() {
    let one = r#"<svg><style>rect.a.b{fill:red}</style><rect class="a"/></svg>"#;
    assert_eq!(resolved(one, &["rect"], "fill"), None);
    let both = r#"<svg><style>rect.a.b{fill:red}</style><rect class="a b"/></svg>"#;
    assert_eq!(resolved(both, &["rect"], "fill").as_deref(), Some("red"));
}

#[test]
fn a_selector_list_applies_to_each_of_its_selectors() {
    let document = r"<svg><style>circle, rect{fill:red}</style><rect/></svg>";
    assert_eq!(
        resolved(document, &["rect"], "fill").as_deref(),
        Some("red")
    );
}

#[test]
fn the_descendant_combinator_reaches_any_ancestor() {
    let document = r"<svg><style>svg rect{fill:red}</style><g><rect/></g></svg>";
    assert_eq!(
        resolved(document, &["g", "rect"], "fill").as_deref(),
        Some("red")
    );
}

#[test]
fn the_child_combinator_needs_the_immediate_parent() {
    let nested = r"<svg><style>svg > rect{fill:red}</style><g><rect/></g></svg>";
    assert_eq!(resolved(nested, &["g", "rect"], "fill"), None);
    let direct = r"<svg><style>svg > rect{fill:red}</style><rect/></svg>";
    assert_eq!(resolved(direct, &["rect"], "fill").as_deref(), Some("red"));
}

/// Matching right to left and taking the nearest ancestor would take the
/// inner `.b`, find its parent is not `.a`, and wrongly fail.
#[test]
fn a_child_combinator_after_a_descendant_one_still_matches() {
    let document = r#"<svg><style>.a > .b .c{fill:red}</style>
        <g id="outer" class="a"><g id="mid" class="b"><g id="inner" class="b">
        <rect id="r" class="c"/></g></g></g></svg>"#;
    assert_eq!(
        resolved(document, &["outer", "mid", "inner", "r"], "fill").as_deref(),
        Some("red")
    );
}

#[test]
fn only_the_subject_may_satisfy_the_last_compound() {
    let document = r#"<svg><style>g g{fill:red}</style>
        <g id="outer"><g id="inner"><rect/></g></g></svg>"#;
    assert_eq!(
        resolved(document, &["outer", "inner"], "fill").as_deref(),
        Some("red")
    );
    assert_eq!(resolved(document, &["outer"], "fill"), None);
    assert_eq!(
        resolved(document, &["outer", "inner", "rect"], "fill"),
        None
    );
}

// --- the cascade -----------------------------------------------------------

#[test]
fn a_more_specific_selector_wins() {
    let document = r#"<svg><style>#r{fill:blue}.c{fill:green}rect{fill:red}</style>
        <rect id="r" class="c"/></svg>"#;
    assert_eq!(resolved(document, &["r"], "fill").as_deref(), Some("blue"));
}

#[test]
fn equal_specificity_is_broken_by_source_order() {
    let document = r"<svg><style>rect{fill:red}rect{fill:green}</style><rect/></svg>";
    assert_eq!(
        resolved(document, &["rect"], "fill").as_deref(),
        Some("green")
    );
}

#[test]
fn an_important_declaration_comes_last_whatever_its_specificity() {
    let document =
        r#"<svg><style>#r{fill:blue}rect{fill:red !important}</style><rect id="r"/></svg>"#;
    assert_eq!(resolved(document, &["r"], "fill").as_deref(), Some("red"));
}

// --- what is read, and what is dropped ------------------------------------

#[test]
fn a_cdata_wrapped_sheet_is_read_verbatim() {
    let document = r"<svg><style><![CDATA[ rect { fill: red } ]]></style><rect/></svg>";
    assert_eq!(
        resolved(document, &["rect"], "fill").as_deref(),
        Some("red")
    );
}

#[test]
fn comments_are_not_declarations() {
    let document = r"<svg><style>/* rect{fill:blue} */ rect{fill:red}</style><rect/></svg>";
    assert_eq!(
        resolved(document, &["rect"], "fill").as_deref(),
        Some("red")
    );
}

/// A sheet is character data, and an XML comment inside it splits that data
/// into runs the element must join back up.
#[test]
fn a_sheet_split_by_an_xml_comment_is_read_whole() {
    let document = "<svg><style>rect{fill:<!-- nothing -->red}</style><rect/></svg>";
    assert_eq!(
        resolved(document, &["rect"], "fill").as_deref(),
        Some("red")
    );
}

#[test]
fn an_unterminated_comment_ends_the_sheet_rather_than_the_document() {
    let document = r"<svg><style>rect{fill:red} /* on and on</style><rect/></svg>";
    assert_eq!(
        resolved(document, &["rect"], "fill").as_deref(),
        Some("red")
    );
}

/// A construct outside the subset does not apply, which is what dropping it
/// says; refusing the document would lose artwork over a rule it would never
/// have drawn. The rule *after* it must still be read.
#[test]
fn an_unsupported_selector_or_at_rule_is_dropped_not_refused() {
    for unsupported in [
        "rect[width]{fill:blue}",
        "rect:hover{fill:blue}",
        "g + rect{fill:blue}",
        "> rect{fill:blue}",
        "@media print{rect{fill:blue}}",
        "@import url(other.css);",
    ] {
        let document = format!("<svg><style>{unsupported} rect{{stroke:red}}</style><rect/></svg>");
        assert_eq!(
            resolved(&document, &["rect"], "fill"),
            None,
            "{unsupported}"
        );
        assert_eq!(
            resolved(&document, &["rect"], "stroke").as_deref(),
            Some("red"),
            "{unsupported} swallowed the rule after it"
        );
    }
}

#[test]
fn a_sheet_for_another_medium_or_language_is_skipped() {
    for attributes in [r#"media="print""#, r#"type="text/plain""#] {
        let document = format!("<svg><style {attributes}>rect{{fill:red}}</style><rect/></svg>");
        assert_eq!(resolved(&document, &["rect"], "fill"), None, "{attributes}");
    }
}

#[test]
fn a_document_with_no_sheet_matches_nothing() {
    let root = xml::parse("<svg><rect/></svg>").expect("a document");
    let sheets = crate::css::sheet_texts(&root);
    assert!(Stylesheet::collect(&sheets).expect("a sheet").is_empty());
}

// --- the bounds ------------------------------------------------------------

#[test]
fn a_sheet_past_the_rule_bound_refuses_the_document() {
    let mut text = String::new();
    for index in 0..1024 {
        let _ = write!(text, ".c{index}{{fill:red}}");
    }
    let document = format!("<svg><style>{text}</style><rect/></svg>");
    let root = xml::parse(&document).expect("a document");
    let sheets = crate::css::sheet_texts(&root);
    assert_eq!(
        Stylesheet::collect(&sheets).err(),
        Some(SvgError::TooComplex)
    );
}

#[test]
fn a_selector_past_the_part_bound_is_dropped() {
    let document = "<svg><style>g g g g g g g g g rect{fill:red}</style><rect/></svg>";
    let root = xml::parse(document).expect("a document");
    let sheets = crate::css::sheet_texts(&root);
    assert!(Stylesheet::collect(&sheets).expect("a sheet").is_empty());
}
