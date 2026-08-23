//! Unit tests for the catalog registry.
//!
//! The reading is the crate's fail-closed boundary against a hostile or
//! corrupted store, so every refusal is pinned deterministically — the
//! randomised harness in `tests/fuzz_proglib.rs` walks the same boundary
//! from generated documents, but a named case per rejection is what a
//! reviewer checks the registry against.
//!
//! The `key = value` grammar itself is `lib/appconf`'s and is tested there;
//! what is tested here is everything the *registry* judges on top of it.

use alloc::format;
use alloc::string::String;

use super::*;
use crate::catalog::EntryPatch;
use crate::entry::{DisplayName, EntryError, EntryId, IconAsset, LibraryEntry, MAX_ENTRY_ID_LEN};

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

/// The catalog `text` holds, read through the format engine and the registry
/// exactly as a store's reader does.
fn read(text: &str) -> Result<Catalog, CatalogError> {
    let document = Document::parse(text).expect("the fixture is a well-formed document");
    load(&document)
}

/// The canonical rendered text of `catalog`.
fn rendered(catalog: &Catalog) -> String {
    document(catalog).render()
}

#[test]
fn a_full_document_reads_to_its_records() {
    let text = "\
# The machine store, with a comment and a blank line.

com.example.editor.name = Text Editor # trailing note
com.example.editor.bundle = /Apps/Editor.app
com.example.editor.category = Office
com.example.editor.icon = editor.svg

chess.hidden = true
chess.name = My Chess
";
    let catalog = read(text).expect("well-formed document");
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
        .insert(entry("editor", LibraryCategory::Office))
        .expect("declared");
    let mut hide = EntryPatch::new();
    hide.set_hidden(false);
    hide.set_category(LibraryCategory::Games);
    catalog.patch(id("chess"), hide).expect("patched");

    assert_eq!(
        rendered(&catalog),
        "chess.category = Games\n\
         chess.hidden = false\n\
         editor.name = editor\n\
         editor.bundle = /Apps/editor.app\n\
         editor.category = Office\n"
    );
    assert_eq!(read(&rendered(&catalog)).expect("re-reads"), catalog);
}

#[test]
fn a_hidden_declaration_round_trips_and_an_explicit_show_normalises_away() {
    let text = "\
editor.name = Editor
editor.bundle = /Apps/Editor.app
editor.hidden = true
chess.name = Chess
chess.bundle = /Apps/Chess.app
chess.hidden = false
";
    let catalog = read(text).expect("hidden declarations are legal");
    assert!(catalog.entry(&id("editor")).expect("entry").hidden());
    assert!(!catalog.entry(&id("chess")).expect("entry").hidden());

    // Visible is the default, so only the suppression earns a setting.
    assert_eq!(
        rendered(&catalog),
        "chess.name = Chess\n\
         chess.bundle = /Apps/Chess.app\n\
         chess.category = Other\n\
         editor.name = Editor\n\
         editor.bundle = /Apps/Editor.app\n\
         editor.category = Other\n\
         editor.hidden = true\n"
    );
    assert_eq!(read(&rendered(&catalog)).expect("re-reads"), catalog);
}

#[test]
fn a_reverse_dns_key_splits_at_its_last_dot() {
    let catalog = read("com.example.editor.name = Editor\n").expect("reads");
    let patch = catalog
        .entry_patch(&id("com.example.editor"))
        .expect("patch");
    assert_eq!(patch.name().map(DisplayName::as_str), Some("Editor"));
}

#[test]
fn every_field_a_record_can_hold_is_a_legal_setting() {
    // `document` drops a field the format engine refuses, so this is what
    // pins that it never has to: every registry key is inside the key
    // grammar, and every value the entry model admits is inside the value
    // grammar. Both are checked against the engine's own definitions.
    let mut catalog = Catalog::new();
    let mut declared = LibraryEntry::new(
        id("com.example.editor"),
        DisplayName::new("Text Editor").expect("name"),
        crate::entry::BundlePath::new("/Apps/Editor.app").expect("bundle"),
        LibraryCategory::Office,
        Some(IconAsset::new("editor.svg").expect("icon")),
    );
    declared.set_hidden(true);
    catalog.insert(declared).expect("declared");

    let document = document(&catalog);
    assert_eq!(document.settings().count(), EntryKey::ALL.len());
    for setting in document.settings() {
        assert_eq!(
            tairix_appconf::validate_key(setting.key),
            Ok(()),
            "{}",
            setting.key
        );
        assert!(setting.value.len() <= tairix_appconf::MAX_VALUE_LEN);
    }
}

#[test]
fn a_line_the_grammar_did_not_read_as_a_setting_is_refused_where_it_stands() {
    // The engine keeps such a line rather than aborting the document; a
    // *catalog* refuses it, because a list with a line nobody understood is
    // a list that may be silently missing a record.
    let mut document = Document::parse("editor.name = Editor\n").expect("well-formed");
    let with_junk = Document::parse(&format!("{}nonsense\n", document.render())).expect("parses");
    document = with_junk;
    let error = load(&document).expect_err("an unparsed line is refused");
    assert_eq!(error.kind(), ParseError::Unparsed);
    assert_eq!(error.line(), Some(2));
}

#[test]
fn a_key_that_is_not_id_dot_field_is_refused() {
    let error = read("editorname = Editor\n").expect_err("no dot");
    assert_eq!(error.kind(), ParseError::MalformedKey);
}

#[test]
fn a_field_outside_the_registry_is_refused() {
    let error = read("editor.colour = mauve\n").expect_err("unknown field");
    assert_eq!(error.kind(), ParseError::UnknownKey);
    assert_eq!(EntryKey::from_id("colour"), None);
}

#[test]
fn a_field_set_twice_for_one_entry_takes_the_last_setting() {
    // The format engine defines what a repeated key means — the last one
    // wins, so appending overrides — and this registry does not get a second
    // opinion about it.
    let catalog = read("editor.name = One\neditor.name = Two\n").expect("reads");
    let patch = catalog.entry_patch(&id("editor")).expect("patch");
    assert_eq!(patch.name().map(DisplayName::as_str), Some("Two"));
}

#[test]
fn a_folder_outside_the_taxonomy_is_refused() {
    for text in ["editor.category = Stuff\n", "editor.category = office\n"] {
        let error = read(text).expect_err("unknown folder");
        assert_eq!(error.kind(), ParseError::UnknownCategory, "{text:?}");
    }
}

#[test]
fn a_flag_that_is_neither_true_nor_false_is_refused() {
    for text in ["editor.hidden = yes\n", "editor.hidden = True\n"] {
        let error = read(text).expect_err("malformed flag");
        assert_eq!(error.kind(), ParseError::MalformedFlag, "{text:?}");
    }
}

#[test]
fn a_field_the_entry_model_refuses_is_a_field_refusal() {
    // An identifier the *key* grammar admits but the entry model does not:
    // a key may be 128 bytes and an identifier 64, so the wider one is the
    // reachable refusal. Anything narrower than the key grammar never reaches
    // the registry at all — the engine keeps the line unparsed instead.
    let long = alloc::format!("{}.name = Editor\n", "a".repeat(MAX_ENTRY_ID_LEN + 1));
    let error = read(&long).expect_err("an identifier past the entry bound");
    assert_eq!(error.kind(), ParseError::Field(EntryError::IdTooLong));

    let error = read("editor.bundle = /Storage/usb0/Editor.app\n").expect_err("hostile bundle");
    assert_eq!(
        error.kind(),
        ParseError::Field(EntryError::MalformedBundlePath)
    );
}

#[test]
fn a_bundle_without_a_display_name_is_refused() {
    let error = read("editor.bundle = /Apps/Editor.app\n").expect_err("incomplete entry");
    assert_eq!(error.kind(), ParseError::IncompleteEntry);
}

#[test]
fn a_catalog_at_the_record_bound_renders_whole() {
    // `document` is total: it drops a field the format engine refuses, so what
    // pins that it never has to is the record bound being derived from what a
    // *full* render costs. Without this, a catalog could be built in memory,
    // saved, and silently lose the applications past the setting bound.
    let mut catalog = Catalog::new();
    for index in 0..MAX_ENTRIES {
        let mut declared = LibraryEntry::new(
            id(&format!("os.tairix.app{index}")),
            DisplayName::new(&format!("App {index}")).expect("name"),
            crate::entry::BundlePath::new(&format!("/Apps/app{index}.app")).expect("bundle"),
            LibraryCategory::Office,
            Some(IconAsset::new("icon.svg").expect("icon")),
        );
        // Every field a record can carry, so the render is the widest one.
        declared.set_hidden(true);
        catalog.insert(declared).expect("fits the record bound");
    }
    assert_eq!(catalog.len(), MAX_ENTRIES);

    let document = document(&catalog);
    assert_eq!(
        document.settings().count(),
        MAX_ENTRIES * EntryKey::ALL.len(),
        "every field of every record survived the render"
    );
    let reread = load(&document).expect("a full catalog re-reads");
    assert_eq!(reread, catalog, "and reads back as the same catalog");

    // One more record does not fit, and is refused where a caller can act on
    // it rather than at save time.
    let mut overflow = Catalog::new();
    for index in 0..MAX_ENTRIES {
        overflow
            .insert(entry(&format!("app{index}"), LibraryCategory::Other))
            .expect("fits");
    }
    assert!(overflow
        .insert(entry("one-too-many", LibraryCategory::Other))
        .is_err());
}

#[test]
fn a_document_holding_more_records_than_the_bound_is_refused() {
    use core::fmt::Write as _;

    // One-setting records are the cheapest a document can carry, so this is
    // the shape that reaches the *record* bound while the format's setting
    // bound still has room — which is exactly why the registry enforces its
    // own rather than inferring it.
    let mut text = String::new();
    for index in 0..=MAX_ENTRIES {
        let _ = writeln!(text, "entry-{index}.name = Entry");
    }
    let error = read(&text).expect_err("past the record bound");
    assert_eq!(error.kind(), ParseError::TooManyEntries);
}

#[test]
fn every_refusal_says_what_was_wrong() {
    for kind in [
        ParseError::Unparsed,
        ParseError::MalformedKey,
        ParseError::UnknownKey,
        ParseError::UnknownCategory,
        ParseError::MalformedFlag,
        ParseError::Field(EntryError::MalformedId),
        ParseError::IncompleteEntry,
        ParseError::TooManyEntries,
    ] {
        assert!(!format!("{kind}").is_empty());
    }
    let document = Document::parse("editor.name = Editor\nnonsense\n").expect("parses");
    let located = load(&document).expect_err("an unparsed line is refused");
    assert_eq!(format!("{located}"), "line 2: line is not a setting");
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
