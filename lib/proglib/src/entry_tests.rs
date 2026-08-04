//! Unit tests for the validated entry model.
//!
//! The validators are the crate's security boundary: every field a catalog
//! carries is untrusted text, so these tests assert each one refuses the
//! whole value rather than sanitising it, and that a value a field accepts
//! is always a value the line-oriented store can re-read.

use alloc::string::String;

use super::*;

fn long(len: usize) -> String {
    core::iter::repeat_n('a', len).collect()
}

#[test]
fn the_users_view_leaf_matches_the_account_home_layout() {
    let home = tairix_users::default_home("ada");
    assert_eq!(home, alloc::format!("/{USERS_VIEW_LEAF}/ada"));
}

#[test]
fn every_program_store_path_definition_is_admitted() {
    let home = tairix_users::default_home("ada");
    let app = tairix_abi::BUNDLE_SUFFIX;
    for path in [
        alloc::format!("{}/Editor{app}", tairix_abi::INSTALLED_APP_STORE),
        alloc::format!("{}/Editor{app}", tairix_abi::SYSTEM_COMMAND_STORE),
        alloc::format!("{}/Editor{app}", tairix_abi::SYSTEM_APPLICATION_STORE),
        alloc::format!("{home}/{}/Editor{app}", tairix_abi::HOME_COMMAND_STORE_DIR),
        alloc::format!(
            "{home}/{}/Editor{app}",
            tairix_abi::HOME_APPLICATION_STORE_DIR
        ),
    ] {
        assert!(
            BundlePath::new(&path).is_ok(),
            "{path:?} names a bundle in a program store"
        );
    }
}

#[test]
fn the_service_store_path_definition_is_refused() {
    let path = alloc::format!(
        "{}/devmgr{}",
        tairix_abi::SYSTEM_SERVICE_STORE,
        tairix_abi::BUNDLE_SUFFIX
    );
    assert_eq!(
        BundlePath::new(&path),
        Err(EntryError::MalformedBundlePath),
        "a daemon is not a launcher-offered program"
    );
}

#[test]
fn an_identifier_admits_a_reverse_dns_spelling() {
    let id = EntryId::new("com.example.editor").expect("reverse-DNS id");
    assert_eq!(id.as_str(), "com.example.editor");
    assert_eq!(alloc::format!("{id}"), "com.example.editor");
}

#[test]
fn an_identifier_admits_the_separators_and_a_single_character() {
    assert!(EntryId::new("a-b_c.d").is_ok());
    assert!(EntryId::new("x").is_ok());
}

#[test]
fn an_empty_identifier_is_refused() {
    assert_eq!(EntryId::new(""), Err(EntryError::EmptyId));
}

#[test]
fn an_over_long_identifier_is_refused() {
    assert!(EntryId::new(&long(MAX_ENTRY_ID_LEN)).is_ok());
    assert_eq!(
        EntryId::new(&long(MAX_ENTRY_ID_LEN + 1)),
        Err(EntryError::IdTooLong)
    );
}

#[test]
fn an_identifier_holding_a_grammar_byte_is_refused() {
    for hostile in ["a b", "a\tb", "a#b", "a\nb.name x", "a/b", "a:b", "é"] {
        assert_eq!(
            EntryId::new(hostile),
            Err(EntryError::MalformedId),
            "{hostile:?} must be refused"
        );
    }
}

#[test]
fn an_identifier_cannot_begin_or_end_with_a_separator() {
    for hostile in [".a", "a.", "-a", "a-", "_a", "a_", "."] {
        assert_eq!(
            EntryId::new(hostile),
            Err(EntryError::MalformedId),
            "{hostile:?} must be refused"
        );
    }
}

#[test]
fn a_display_name_admits_ordinary_text() {
    let name = DisplayName::new("Text Editor").expect("plain name");
    assert_eq!(name.as_str(), "Text Editor");
    assert_eq!(alloc::format!("{name}"), "Text Editor");
}

#[test]
fn an_empty_display_name_is_refused() {
    assert_eq!(DisplayName::new(""), Err(EntryError::EmptyName));
}

#[test]
fn an_over_long_display_name_is_refused() {
    assert!(DisplayName::new(&long(MAX_DISPLAY_NAME_LEN)).is_ok());
    assert_eq!(
        DisplayName::new(&long(MAX_DISPLAY_NAME_LEN + 1)),
        Err(EntryError::NameTooLong)
    );
}

#[test]
fn a_display_name_that_would_forge_a_line_is_refused() {
    for hostile in [
        "Ed\nother.name Evil",
        "Ed\r\nother.name Evil",
        "Ed # comment",
        "Ed\u{0}",
        " Ed",
        "Ed ",
    ] {
        assert_eq!(
            DisplayName::new(hostile),
            Err(EntryError::MalformedName),
            "{hostile:?} must be refused"
        );
    }
}

#[test]
fn a_bundle_path_admits_every_program_store_and_plain_nesting() {
    for path in [
        "/Apps/Editor.app",
        "/Apps/games/chess.app",
        "/Apps/games/board/chess.app",
        "/Users/ada/Commands/tally.app",
        "/Users/ada/Commands/games/tally.app",
        "/Users/ada/Applications/Editor.app",
        "/Users/ada/Applications/games/chess.app",
        "/System/Commands/ps.app",
        "/System/Commands/extras/ps.app",
        "/System/Applications/files.app",
        "/System/Applications/extras/files.app",
    ] {
        let bundle = BundlePath::new(path).expect("permitted bundle path");
        assert_eq!(bundle.as_str(), path);
        assert_eq!(alloc::format!("{bundle}"), path);
    }
}

#[test]
fn a_bundle_path_outside_a_program_store_is_refused() {
    for hostile in [
        "/System/Services/devmgr.app",
        "/System/Drivers/storage/emmc2.app",
        "/Storage/usb0/Editor.app",
        "/Users/ada/Documents/Editor.app",
        "/Users/ada/Editor.app",
        "/Editor.app",
        "/Apps",
        "/System/Commands",
        "/System/Applications",
        "/Users/ada/Commands",
        "/Users/ada/Applications",
        "",
    ] {
        assert_eq!(
            BundlePath::new(hostile),
            Err(EntryError::MalformedBundlePath),
            "{hostile:?} must be refused"
        );
    }
}

#[test]
fn a_directory_that_merely_starts_like_a_store_is_refused() {
    for hostile in [
        "/AppsEvil/Editor.app",
        "/SystemEvil/Commands/Editor.app",
        "/System/CommandsEvil/Editor.app",
        "/System/ApplicationsEvil/Editor.app",
        "/UsersEvil/ada/Commands/Editor.app",
        "/Users/ada/CommandsEvil/Editor.app",
        "/Users/ada/ApplicationsEvil/Editor.app",
        "/Users/ada/nested/Commands/Editor.app",
        "/Users/Commands/Editor.app",
        "/System/Editor.app",
    ] {
        assert_eq!(
            BundlePath::new(hostile),
            Err(EntryError::MalformedBundlePath),
            "{hostile:?} must be refused"
        );
    }
}

#[test]
fn a_bundle_path_must_name_a_bundle_directory() {
    for hostile in [
        "/Apps/Editor",
        "/Apps/.app",
        "/Apps/Editor.app/Run",
        "/Apps//Editor.app",
    ] {
        assert_eq!(
            BundlePath::new(hostile),
            Err(EntryError::MalformedBundlePath),
            "{hostile:?} must be refused"
        );
    }
}

#[test]
fn a_bundle_path_cannot_nest_a_bundle_inside_a_bundle() {
    assert_eq!(
        BundlePath::new("/Apps/Suite.app/Editor.app"),
        Err(EntryError::MalformedBundlePath)
    );
}

#[test]
fn a_traversal_is_normalised_and_judged_by_where_it_lands() {
    let inside = BundlePath::new("/Apps/games/../Editor.app").expect("lands in the store");
    assert_eq!(inside.as_str(), "/Apps/Editor.app");
    assert_eq!(
        BundlePath::new("/Apps/../Storage/Editor.app"),
        Err(EntryError::MalformedBundlePath)
    );
}

#[test]
fn redundant_spellings_of_one_bundle_collapse_to_one_value() {
    let canonical = BundlePath::new("/Apps/Editor.app").expect("permitted bundle path");
    for spelling in [
        "/Apps/./Editor.app",
        "/Apps/games/../Editor.app",
        "/Apps/games/board/../../Editor.app",
    ] {
        let bundle = BundlePath::new(spelling).expect("permitted bundle path");
        assert_eq!(bundle.as_str(), "/Apps/Editor.app", "{spelling:?}");
        assert_eq!(bundle, canonical, "{spelling:?}");
    }
}

#[test]
fn a_relative_or_alias_rooted_bundle_path_is_refused() {
    for hostile in [
        "Apps/Editor.app",
        "System:/Apps/Editor.app",
        "Apps:/Editor.app",
    ] {
        assert_eq!(
            BundlePath::new(hostile),
            Err(EntryError::MalformedBundlePath),
            "{hostile:?} must be refused"
        );
    }
}

#[test]
fn an_over_long_bundle_path_is_refused_before_it_is_parsed() {
    let hostile = alloc::format!("/Apps/{}.app", long(MAX_BUNDLE_PATH_LEN));
    assert_eq!(
        BundlePath::new(&hostile),
        Err(EntryError::BundlePathTooLong)
    );
}

#[test]
fn an_icon_asset_admits_a_plain_file_name() {
    let icon = IconAsset::new("icon.svg").expect("plain file name");
    assert_eq!(icon.as_str(), "icon.svg");
    assert_eq!(alloc::format!("{icon}"), "icon.svg");
}

#[test]
fn an_icon_asset_that_escapes_its_bundle_is_refused() {
    for hostile in [
        "",
        ".",
        "..",
        "../../System/Security/Users",
        "sub/icon.svg",
        "/Apps/Other.app/Resources/icon.svg",
        "icon\nother.icon evil.svg",
        "Alias:icon.svg",
    ] {
        assert_eq!(
            IconAsset::new(hostile),
            Err(EntryError::MalformedIconAsset),
            "{hostile:?} must be refused"
        );
    }
}

#[test]
fn an_icon_asset_that_would_start_a_comment_is_refused() {
    assert_eq!(
        IconAsset::new("icon#1.svg"),
        Err(EntryError::MalformedIconAsset)
    );
}

#[test]
fn an_over_long_icon_asset_is_refused() {
    assert!(IconAsset::new(&long(MAX_ICON_ASSET_LEN)).is_ok());
    assert_eq!(
        IconAsset::new(&long(MAX_ICON_ASSET_LEN + 1)),
        Err(EntryError::IconAssetTooLong)
    );
}

#[test]
fn a_renderable_value_is_one_the_store_can_re_read() {
    assert!(value_is_renderable("Text Editor"));
    assert!(!value_is_renderable("has # comment"));
    assert!(!value_is_renderable("has\nbreak"));
    assert!(!value_is_renderable(" padded "));
}

#[test]
fn every_field_refusal_says_what_was_wrong() {
    for error in [
        EntryError::EmptyId,
        EntryError::IdTooLong,
        EntryError::MalformedId,
        EntryError::EmptyName,
        EntryError::NameTooLong,
        EntryError::MalformedName,
        EntryError::BundlePathTooLong,
        EntryError::MalformedBundlePath,
        EntryError::IconAssetTooLong,
        EntryError::MalformedIconAsset,
    ] {
        assert!(!alloc::format!("{error}").is_empty());
    }
}

fn entry() -> LibraryEntry {
    LibraryEntry::new(
        EntryId::new("com.example.editor").expect("id"),
        DisplayName::new("Editor").expect("name"),
        BundlePath::new("/Apps/Editor.app").expect("bundle"),
        LibraryCategory::Accessories,
        None,
    )
}

#[test]
fn an_entry_reads_back_the_fields_it_was_built_from() {
    let entry = entry();
    assert_eq!(entry.id().as_str(), "com.example.editor");
    assert_eq!(entry.name().as_str(), "Editor");
    assert_eq!(entry.bundle().as_str(), "/Apps/Editor.app");
    assert_eq!(entry.category(), LibraryCategory::Accessories);
    assert!(entry.icon().is_none());
    assert!(!entry.hidden(), "an entry is visible until suppressed");
}

#[test]
fn a_suppression_flips_only_the_visibility_flag() {
    let mut entry = entry();
    entry.set_hidden(true);
    assert!(entry.hidden());
    assert_eq!(entry.name().as_str(), "Editor");

    entry.set_hidden(false);
    assert!(!entry.hidden());
}

#[test]
fn a_rename_a_re_file_and_a_re_icon_replace_only_their_own_field() {
    let mut entry = entry();
    entry.set_name(DisplayName::new("Notes").expect("name"));
    entry.set_category(LibraryCategory::Office);
    entry.set_icon(IconAsset::new("notes.svg").expect("icon"));

    assert_eq!(entry.name().as_str(), "Notes");
    assert_eq!(entry.category(), LibraryCategory::Office);
    assert_eq!(entry.icon().map(IconAsset::as_str), Some("notes.svg"));
    assert_eq!(entry.id().as_str(), "com.example.editor");
    assert_eq!(entry.bundle().as_str(), "/Apps/Editor.app");
}
