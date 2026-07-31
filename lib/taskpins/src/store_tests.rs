//! Unit tests for the pin store document grammar.

use core::fmt::Write as _;

use super::*;
use crate::pin::{PinList, PinTarget};
use tairix_proglib::{BundlePath, EntryId};

fn id(text: &str) -> EntryId {
    EntryId::new(text).expect("test identifier")
}

fn bundle(path: &str) -> BundlePath {
    BundlePath::new(path).expect("test bundle path")
}

#[test]
fn a_full_document_parses_to_its_pins() {
    let text = "\
# The pin store, with a comment and a blank line.

entry com.example.editor # trailing note
bundle /Apps/Custom.app

entry chess
";
    let list = parse(text).expect("well-formed document");
    assert_eq!(list.len(), 3);

    assert_eq!(
        list.get(0),
        Some(&PinTarget::Entry(id("com.example.editor")))
    );
    assert_eq!(
        list.get(1),
        Some(&PinTarget::Bundle(bundle("/Apps/Custom.app")))
    );
    assert_eq!(list.get(2), Some(&PinTarget::Entry(id("chess"))));
}

#[test]
fn the_render_is_canonical_and_round_trips() {
    let mut list = PinList::new();
    list.pin(PinTarget::Entry(id("Editor"))).unwrap();
    list.pin(PinTarget::Bundle(bundle("/Apps/Custom.app")))
        .unwrap();

    let rendered = render(&list);
    assert_eq!(
        rendered,
        "entry Editor\n\
         bundle /Apps/Custom.app\n"
    );
    assert_eq!(parse(&rendered).expect("re-parses"), list);
}

#[test]
fn over_long_document_is_refused() {
    let mut text = String::new();
    for _ in 0..=MAX_PINS {
        let _ = writeln!(text, "entry a");
    }
    // Ensure it exceeds MAX_PINS_LEN even if lines are short
    let mut long_text = String::with_capacity(MAX_PINS_LEN + 1);
    while long_text.len() <= MAX_PINS_LEN {
        long_text.push(' ');
    }

    let err = parse(&long_text).unwrap_err();
    assert_eq!(err.line(), None);
    assert_eq!(err.kind(), ParseError::DocumentTooLong);
}

#[test]
fn over_long_line_is_refused() {
    let mut line = String::from("entry ");
    while line.len() <= MAX_LINE_LEN {
        line.push('a');
    }
    let err = parse(&line).unwrap_err();
    assert_eq!(err.line(), Some(1));
    assert_eq!(err.kind(), ParseError::LineTooLong);
}

#[test]
fn missing_value_is_refused() {
    let text = "entry\n";
    let err = parse(text).unwrap_err();
    assert_eq!(err.line(), Some(1));
    assert_eq!(err.kind(), ParseError::MissingValue);

    let text = "bundle  \n";
    let err = parse(text).unwrap_err();
    assert_eq!(err.line(), Some(1));
    assert_eq!(err.kind(), ParseError::MissingValue);
}

#[test]
fn unknown_key_is_refused() {
    let text = "unknown value\n";
    let err = parse(text).unwrap_err();
    assert_eq!(err.line(), Some(1));
    assert_eq!(err.kind(), ParseError::UnknownKey);
}

#[test]
fn duplicate_pin_is_refused() {
    let text = "entry e1\nentry e1\n";
    let err = parse(text).unwrap_err();
    assert_eq!(err.line(), Some(2));
    assert_eq!(err.kind(), ParseError::DuplicatePin);
}

#[test]
fn too_many_pins_is_refused() {
    let mut text = String::new();
    for i in 0..MAX_PINS {
        let _ = writeln!(text, "entry e{i}");
    }
    parse(&text).expect("at limit");

    let _ = writeln!(text, "entry too_many");
    let err = parse(&text).unwrap_err();
    assert_eq!(err.line(), Some(MAX_PINS + 1));
    assert_eq!(err.kind(), ParseError::TooManyPins);
}

#[test]
fn invalid_target_id_is_refused() {
    let text = "entry .\n"; // EntryId cannot start with .
    let err = parse(text).unwrap_err();
    assert_eq!(err.line(), Some(1));
    match err.kind() {
        ParseError::Target(EntryError::MalformedId) => {}
        other => panic!("unexpected error kind: {other:?}"),
    }
}

#[test]
fn invalid_target_bundle_is_refused() {
    let text = "bundle not-absolute\n";
    let err = parse(text).unwrap_err();
    assert_eq!(err.line(), Some(1));
    match err.kind() {
        ParseError::Target(EntryError::MalformedBundlePath) => {}
        other => panic!("unexpected error kind: {other:?}"),
    }
}

#[test]
fn comments_and_blank_lines_are_ignored() {
    let text = "\
# comment
   # indented comment

entry e1

   
bundle /Apps/A.app
";
    let list = parse(text).expect("ignored whitespace");
    assert_eq!(list.len(), 2);
    assert_eq!(list.get(0), Some(&PinTarget::Entry(id("e1"))));
    assert_eq!(list.get(1), Some(&PinTarget::Bundle(bundle("/Apps/A.app"))));
}
