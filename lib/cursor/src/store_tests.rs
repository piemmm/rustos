//! Tests for the shipped cursor-set store's naming, paths, and listing.
//!
//! The crate is `no_std`, but a test module may use `std`: the shipped-set
//! walk reads the crate's own `assets/` directory off the host filesystem,
//! so what actually ships is held to its own contract rather than to a
//! copied list.

extern crate std;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::desktop::{CURSOR_SETS_MAX, CURSOR_SET_NAME_MAX};
use tairix_theme::{CursorKind, CursorSetId, CURSOR_KINDS};

use super::{
    catalog_sets, cursor_asset_kind_for_file, cursor_asset_path, is_cursor_set_name, set_path,
    CURSOR_STORE, SHIPPED_CURSOR_SET,
};

/// A set id from a name a test knows is legal.
fn id(name: &str) -> CursorSetId {
    CursorSetId::new(name).expect("a legal set name")
}

#[test]
fn a_set_path_is_its_directory_in_the_store() {
    assert_eq!(
        set_path(id("High Visibility")),
        "/System/Graphics/Cursors/High Visibility"
    );
    assert!(set_path(id("X")).starts_with(CURSOR_STORE));
}

#[test]
fn an_asset_path_is_the_set_then_the_theme_s_own_asset_id() {
    assert_eq!(
        cursor_asset_path(id("High Visibility"), CursorKind::Arrow.asset_id()).as_deref(),
        Some("/System/Graphics/Cursors/High Visibility/cursor.arrow.svg")
    );
    // A theme naming artwork of its own resolves inside the chosen set.
    assert_eq!(
        cursor_asset_path(id("High Visibility"), "vendor.arrow").as_deref(),
        Some("/System/Graphics/Cursors/High Visibility/vendor.arrow.svg")
    );
}

/// An asset id is *theme* data reaching a path, so the property that
/// matters is that no id can spell a path outside its own set directory.
///
/// An id that would need a separator to escape is refused outright; an odd
/// one that cannot escape still resolves, because the suffix is always
/// appended — so the name always ends `.svg` and is never `.` or `..`.
#[test]
fn no_asset_id_can_spell_a_path_outside_its_own_set() {
    let set = id("High Visibility");
    let inside = alloc::format!("{}/", set_path(set));
    for asset_id in [
        "../../../Users/ada/Documents/secret",
        "nested/arrow",
        "/etc/shadow",
        "a\u{7f}b",
    ] {
        assert_eq!(
            cursor_asset_path(set, asset_id),
            None,
            "`{asset_id}` must not spell a store path"
        );
    }
    for asset_id in ["..", ".", "", "cursor.arrow"] {
        let path = cursor_asset_path(set, asset_id)
            .unwrap_or_else(|| panic!("`{asset_id}` names a file inside the set"));
        assert!(
            path.starts_with(&inside) && !path[inside.len()..].contains('/'),
            "`{asset_id}` resolved to `{path}`, outside its own set"
        );
    }
}

#[test]
fn the_shipped_set_is_a_legal_set_name() {
    assert!(is_cursor_set_name(SHIPPED_CURSOR_SET));
    assert_ne!(SHIPPED_CURSOR_SET, CursorSetId::BUILTIN_NAME);
}

/// A name with a separator would widen the store path it is spliced into,
/// so it is not a name any set may carry.
#[test]
fn a_name_that_could_widen_a_path_is_refused() {
    for name in ["", ".", "..", "a/b", "../../System", "a\u{7f}b", "C:"] {
        assert!(!is_cursor_set_name(name), "`{name}` must not name a set");
    }
}

#[test]
fn a_name_is_refused_past_the_wire_bound() {
    let widest = "s".repeat(CURSOR_SET_NAME_MAX);
    assert!(is_cursor_set_name(&widest));
    assert!(!is_cursor_set_name(&"s".repeat(CURSOR_SET_NAME_MAX + 1)));
    assert_eq!(id(&widest).name(), widest);
}

#[test]
fn every_kind_s_own_asset_name_resolves_back_to_that_kind() {
    for kind in CURSOR_KINDS {
        let file = alloc::format!("{}.svg", kind.asset_id());
        assert_eq!(cursor_asset_kind_for_file(&file), Some(kind));
    }
}

#[test]
fn an_asset_name_no_kind_asks_for_is_refused() {
    for name in [
        "",
        "cursor.arrow",
        "cursor.arrow.png",
        "cursor.unknown.svg",
        "../cursor.arrow.svg",
        ".svg",
    ] {
        assert_eq!(
            cursor_asset_kind_for_file(name),
            None,
            "`{name}` must not name a cursor asset"
        );
    }
}

#[test]
fn a_listing_yields_its_legal_sets_in_name_order() {
    let sets = catalog_sets(["Zephyr", "High Visibility", "Amber"]);
    let names: Vec<_> = sets.iter().map(|set| set.name().to_string()).collect();
    assert_eq!(names, ["Amber", "High Visibility", "Zephyr"]);
}

/// A store mixing sets with names no set may carry yields only the sets,
/// rather than refusing the whole listing.
#[test]
fn an_unusable_name_is_dropped_rather_than_refusing_the_listing() {
    let sets = catalog_sets([
        "Amber",
        "..",
        "a/b",
        "",
        &"s".repeat(CURSOR_SET_NAME_MAX + 1),
    ]);
    assert_eq!(sets.len(), 1);
    assert_eq!(sets[0].name(), "Amber");
}

/// The built-in set is offered beside whatever the store carries, so a
/// directory under its name could otherwise shadow it.
#[test]
fn a_directory_claiming_the_builtin_name_is_dropped() {
    let sets = catalog_sets([CursorSetId::BUILTIN_NAME, "Amber"]);
    assert_eq!(sets.len(), 1);
    assert_eq!(sets[0].name(), "Amber");
}

#[test]
fn a_repeated_name_is_offered_once() {
    let sets = catalog_sets(["Amber", "Amber"]);
    assert_eq!(sets.len(), 1);
}

/// The listing leaves room for the built-in set, so the whole choice space
/// still fits the one reply frame that carries it.
#[test]
fn the_listing_leaves_the_builtin_its_slot() {
    let names: Vec<alloc::string::String> = (0..CURSOR_SETS_MAX + 8)
        .map(|n| alloc::format!("set{n:03}"))
        .collect();
    let sets = catalog_sets(names.iter().map(alloc::string::String::as_str));
    assert_eq!(sets.len(), CURSOR_SETS_MAX - 1);
    assert_eq!(sets[0].name(), "set000");
}

/// The shipped sets, read from the crate's own `assets/` directory: every
/// subdirectory name, in name order.
fn shipped_sets() -> Vec<String> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/assets");
    let mut sets: Vec<String> = std::fs::read_dir(dir)
        .expect("the shipped assets directory")
        .map(|entry| entry.expect("directory entry"))
        .filter(|entry| entry.path().is_dir())
        .map(|entry| {
            entry
                .file_name()
                .into_string()
                .expect("utf-8 set directory name")
        })
        .collect();
    sets.sort();
    sets
}

/// One shipped set's assets: `(file name, bytes)` in name order.
fn shipped_assets(set: &str) -> Vec<(String, std::vec::Vec<u8>)> {
    let dir = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/assets")).join(set);
    let mut assets: Vec<(String, std::vec::Vec<u8>)> = std::fs::read_dir(dir)
        .expect("a shipped set directory")
        .map(|entry| entry.expect("directory entry"))
        .map(|entry| {
            let name = entry.file_name().into_string().expect("utf-8 asset name");
            (name, std::fs::read(entry.path()).expect("asset bytes"))
        })
        .collect();
    assets.sort_by(|a, b| a.0.cmp(&b.0));
    assets
}

/// A desktop whose only set is the built-in one offers a choice of one,
/// which is the control that changes nothing — so the store must actually
/// carry the set the crate names.
#[test]
fn the_named_shipped_set_is_what_the_crate_ships() {
    let sets = shipped_sets();
    assert!(
        sets.iter().any(|set| set == SHIPPED_CURSOR_SET),
        "`{SHIPPED_CURSOR_SET}` is not among the shipped sets {sets:?}"
    );
    for set in &sets {
        assert!(
            is_cursor_set_name(set),
            "shipped set `{set}` is not a name a chooser could offer"
        );
    }
    // Every shipped set must survive the listing it will be offered
    // through, or it is artwork nothing can reach.
    let offered = catalog_sets(sets.iter().map(String::as_str));
    assert_eq!(
        offered.len(),
        sets.len(),
        "a shipped set would not be listed"
    );
}

/// A set missing a kind falls back to that kind's built-in cursor, which is
/// a different drawing beside the shipped ones — so a shipped set covers
/// every kind or the pointer changes look as it changes shape.
#[test]
fn every_shipped_set_covers_every_kind_within_the_byte_bound() {
    for set in shipped_sets() {
        let assets = shipped_assets(&set);
        let mut covered: Vec<CursorKind> = Vec::new();
        for (name, bytes) in &assets {
            let kind = cursor_asset_kind_for_file(name)
                .unwrap_or_else(|| panic!("`{set}/{name}` is not an asset name any kind asks for"));
            assert!(
                bytes.len() <= super::MAX_CURSOR_ASSET_BYTES,
                "`{set}/{name}` is {} bytes, over the {}-byte bound",
                bytes.len(),
                super::MAX_CURSOR_ASSET_BYTES
            );
            covered.push(kind);
        }
        for kind in CURSOR_KINDS {
            assert!(
                covered.contains(&kind),
                "shipped set `{set}` has no artwork for {kind:?}"
            );
        }
    }
}

/// Artwork the decoder refuses, or that draws nothing, would silently show
/// as the built-in cursor — so each shipped asset is decoded here rather
/// than trusted.
#[test]
fn every_shipped_asset_decodes_into_a_cursor_that_draws() {
    for set in shipped_sets() {
        for (name, bytes) in shipped_assets(&set) {
            let cursor = crate::decode_svg(&bytes)
                .unwrap_or_else(|err| panic!("`{set}/{name}` does not decode: {err:?}"));
            let image = cursor
                .rasterise(super::CURSOR_BASE_SIDE_PX)
                .unwrap_or_else(|| panic!("`{set}/{name}` does not rasterise"));
            assert!(
                image.surface().pixels().iter().any(|pixel| pixel.a > 0),
                "`{set}/{name}` draws nothing"
            );
            let hotspot = image.hotspot();
            let side = i32::try_from(super::CURSOR_BASE_SIDE_PX).expect("a small side");
            assert!(
                hotspot.x >= 0 && hotspot.x < side && hotspot.y >= 0 && hotspot.y < side,
                "`{set}/{name}` puts its hotspot at {hotspot:?}, outside its own artwork"
            );
        }
    }
}
