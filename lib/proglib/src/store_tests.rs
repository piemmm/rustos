//! Unit tests for the catalog document grammar.
//!
//! The parser is the crate's fail-closed boundary against a hostile or
//! corrupted store, so every refusal is pinned deterministically — the
//! randomised harness in `tests/fuzz_proglib.rs` walks the same boundary
//! from generated documents, but a named case per rejection is what a
//! reviewer checks the grammar against.

use alloc::format;
use alloc::string::String;

use super::*;
use crate::catalog::EntryPatch;
use crate::entry::{DisplayName, EntryError, EntryId, IconAsset, LibraryEntry};

fn id(text: &str) -> EntryId {
    EntryId::new(text).expect("test identifier")
}

fn entry(leaf: &str, category: LibraryCategory) -> LibraryEntry {
    LibraryEntry::new(
        id(leaf),
        DisplayName::new(leaf).expect("name"),
        crate::entry::BundlePath::new(&format!("/Apps/{leaf}.app")).expect("bundle"),
        category,
        None,
    )
}

#[test]
fn a_full_document_parses_to_its_records() {
    let text = "\
# The machine store, with a comment and a blank line.

com.example.editor.name Text Editor # trailing note
com.example.editor.bundle /Apps/Editor.app
com.example.editor.category Office
com.example.editor.icon editor.svg

chess.hidden true
chess.name My Chess
";
    let catalog = parse(text).expect("well-formed document");
    assert_eq!(catalog.len(), 2);

    let editor = catalog.entry(&id("com.example.editor")).expect("entry");
    assert_eq!(editor.name().as_str(), "Text Editor");
    assert_eq!(editor.bundle().as_str(), "/Apps/Editor.app");
    assert_eq!(editor.category(), LibraryCategory::Office);
    assert_eq!(editor.icon().map(IconAsset::as_str), Some("editor.svg"));
    assert!(!editor.hidden());

    let patch = catalog.entry_patch(&id("chess")).expect("patch");
    assert_eq!(patch.hidden(), Some(true));
    assert_eq!(patch.name().map(DisplayName::as_str), Some("My Chess"));
}

#[test]
fn the_render_is_canonical_and_round_trips() {
    let mut catalog = Catalog::new();
    catalog
        .insert(entry("Editor", LibraryCategory::Office))
        .expect("declared");
    let mut hide = EntryPatch::new();
    hide.set_hidden(false);
    hide.set_category(LibraryCategory::Games);
    catalog.patch(id("chess"), hide).expect("patched");

    let rendered = render(&catalog);
    assert_eq!(
        rendered,
        "Editor.name Editor\n\
         Editor.bundle /Apps/Editor.app\n\
         Editor.category Office\n\
         chess.category Games\n\
         chess.hidden false\n"
    );
    assert_eq!(parse(&rendered).expect("re-parses"), catalog);
}

#[test]
fn a_hidden_declaration_round_trips_and_an_explicit_show_normalises_away() {
    let text = "\
Editor.name Editor
Editor.bundle /Apps/Editor.app
Editor.hidden true
Chess.name Chess
Chess.bundle /Apps/Chess.app
Chess.hidden false
";
    let catalog = parse(text).expect("hidden declarations are legal");
    assert!(catalog.entry(&id("Editor")).expect("entry").hidden());
    assert!(!catalog.entry(&id("Chess")).expect("entry").hidden());

    // Visible is the default, so only the suppression earns a line.
    let rendered = render(&catalog);
    assert_eq!(
        rendered,
        "Chess.name Chess\n\
         Chess.bundle /Apps/Chess.app\n\
         Chess.category Other\n\
         Editor.name Editor\n\
         Editor.bundle /Apps/Editor.app\n\
         Editor.category Other\n\
         Editor.hidden true\n"
    );
    assert_eq!(parse(&rendered).expect("re-parses"), catalog);
}

#[test]
fn a_reverse_dns_key_splits_at_its_last_dot() {
    let catalog = parse("com.example.editor.name Editor\n").expect("parses");
    let patch = catalog
        .entry_patch(&id("com.example.editor"))
        .expect("patch");
    assert_eq!(patch.name().map(DisplayName::as_str), Some("Editor"));
}

#[test]
fn an_over_long_document_is_refused_whole() {
    let mut text = String::with_capacity(MAX_CATALOG_LEN + 1);
    while text.len() <= MAX_CATALOG_LEN {
        text.push_str("# padding\n");
    }
    let error = parse(&text).expect_err("too long");
    assert_eq!(error.kind(), ParseError::DocumentTooLong);
    assert_eq!(error.line(), None);
}

#[test]
fn an_over_long_line_is_refused_where_it_stands() {
    let mut text = String::from("editor.name Editor\n");
    text.push_str(&"a".repeat(MAX_LINE_LEN + 1));
    text.push('\n');
    let error = parse(&text).expect_err("line too long");
    assert_eq!(error.kind(), ParseError::LineTooLong);
    assert_eq!(error.line(), Some(2));
}

#[test]
fn a_setting_without_a_value_is_refused() {
    for text in ["editor.name\n", "editor.name  \n", "editor.name # note\n"] {
        let error = parse(text).expect_err("no value");
        assert_eq!(error.kind(), ParseError::MissingValue, "{text:?}");
        assert_eq!(error.line(), Some(1));
    }
}

#[test]
fn a_key_that_is_not_id_dot_field_is_refused() {
    let error = parse("editorname Editor\n").expect_err("no dot");
    assert_eq!(error.kind(), ParseError::MalformedKey);
}

#[test]
fn a_field_outside_the_registry_is_refused() {
    let error = parse("editor.colour mauve\n").expect_err("unknown field");
    assert_eq!(error.kind(), ParseError::UnknownKey);
    assert_eq!(EntryKey::from_id("colour"), None);
}

#[test]
fn a_field_set_twice_for_one_entry_is_refused() {
    let error = parse("editor.name One\neditor.name Two\n").expect_err("duplicate");
    assert_eq!(error.kind(), ParseError::DuplicateKey);
    assert_eq!(error.line(), Some(2));
}

#[test]
fn a_folder_outside_the_taxonomy_is_refused() {
    for text in ["editor.category Stuff\n", "editor.category office\n"] {
        let error = parse(text).expect_err("unknown folder");
        assert_eq!(error.kind(), ParseError::UnknownCategory, "{text:?}");
    }
}

#[test]
fn a_flag_that_is_neither_true_nor_false_is_refused() {
    for text in ["editor.hidden yes\n", "editor.hidden True\n"] {
        let error = parse(text).expect_err("malformed flag");
        assert_eq!(error.kind(), ParseError::MalformedFlag, "{text:?}");
    }
}

#[test]
fn a_field_the_entry_model_refuses_is_a_field_refusal() {
    let error = parse("bad!.name Editor\n").expect_err("hostile id");
    assert_eq!(error.kind(), ParseError::Field(EntryError::MalformedId));

    let error = parse("editor.bundle /Storage/usb0/Editor.app\n").expect_err("hostile bundle");
    assert_eq!(
        error.kind(),
        ParseError::Field(EntryError::MalformedBundlePath)
    );
}

#[test]
fn a_bundle_without_a_display_name_is_refused_at_its_block() {
    let text = "# leading comment\neditor.bundle /Apps/Editor.app\n";
    let error = parse(text).expect_err("incomplete entry");
    assert_eq!(error.kind(), ParseError::IncompleteEntry);
    assert_eq!(error.line(), Some(2), "points at the block's first line");
}

#[test]
fn a_document_holding_more_records_than_the_bound_is_refused() {
    use core::fmt::Write as _;

    let mut text = String::new();
    for index in 0..=MAX_ENTRIES {
        let _ = writeln!(text, "entry-{index}.name Entry");
    }
    let error = parse(&text).expect_err("too many records");
    assert_eq!(error.kind(), ParseError::TooManyEntries);
    assert_eq!(error.line(), Some(MAX_ENTRIES + 1));
}

#[test]
fn every_refusal_says_what_was_wrong() {
    for kind in [
        ParseError::DocumentTooLong,
        ParseError::LineTooLong,
        ParseError::MissingValue,
        ParseError::MalformedKey,
        ParseError::UnknownKey,
        ParseError::DuplicateKey,
        ParseError::UnknownCategory,
        ParseError::MalformedFlag,
        ParseError::Field(EntryError::MalformedId),
        ParseError::IncompleteEntry,
        ParseError::TooManyEntries,
    ] {
        assert!(!format!("{kind}").is_empty());
    }
    let located = parse("editor.name One\neditor.name Two\n").expect_err("duplicate");
    assert_eq!(
        format!("{located}"),
        "line 2: field is set twice for one entry"
    );
}

#[test]
fn every_registry_key_round_trips_its_spelling() {
    for key in EntryKey::ALL {
        assert_eq!(EntryKey::from_id(key.as_str()), Some(key));
        assert_eq!(format!("{key}"), key.as_str());
    }
    assert_eq!(
        EntryKey::from_id("Name"),
        None,
        "the registry is case-exact"
    );
}
