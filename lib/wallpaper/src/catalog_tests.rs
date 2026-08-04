//! Unit tests for the wallpaper catalog listing model.
//!
//! The crate is `no_std`, but a test module may use `std`: the shipped-master
//! walk reads the crate's own `assets/` directory off the host filesystem, so
//! the bound is checked against the real files rather than a copied number.

extern crate std;

use super::*;

#[test]
fn default_wallpaper_path_is_inside_the_store() {
    assert_eq!(
        default_wallpaper_path(),
        "/System/Graphics/Wallpapers/tairix-dark.jpg"
    );
    assert!(default_wallpaper_path().starts_with(WALLPAPER_STORE));
}

/// The shipped masters, measured from the crate's own `assets/` directory:
/// `(name, byte length)` for each file, in name order.
fn shipped_masters() -> alloc::vec::Vec<(String, usize)> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/assets");
    let mut masters: alloc::vec::Vec<(String, usize)> = std::fs::read_dir(dir)
        .expect("the shipped assets directory")
        .map(|entry| {
            let entry = entry.expect("directory entry");
            let name = entry.file_name().into_string().expect("utf-8 asset name");
            let bytes = usize::try_from(entry.metadata().expect("asset metadata").len())
                .expect("asset size fits usize");
            (name, bytes)
        })
        .collect();
    masters.sort();
    masters
}

#[test]
fn every_shipped_master_is_offerable_and_fits_within_the_byte_bound() {
    let masters = shipped_masters();
    assert!(!masters.is_empty(), "the crate ships wallpaper masters");
    for (name, bytes) in &masters {
        assert!(
            is_wallpaper_file_name(name),
            "shipped master {name} is not an offerable wallpaper file name"
        );
        assert!(
            *bytes <= MAX_WALLPAPER_BYTES,
            "shipped master {name} is {bytes} bytes, over the {MAX_WALLPAPER_BYTES}-byte bound"
        );
    }
    // Every shipped master must survive the catalog it will be listed
    // through: a master the desktop would silently drop is a shipped asset
    // no user could ever choose.
    let listing = catalog_entries(masters.iter().map(|(name, bytes)| (name.as_str(), *bytes)));
    assert_eq!(listing.len(), masters.len());
}

#[test]
fn the_byte_bound_admits_the_largest_shipped_master_with_headroom() {
    let largest = shipped_masters()
        .into_iter()
        .map(|(_, bytes)| bytes)
        .max()
        .expect("the crate ships wallpaper masters");
    // Headroom, not a bare fit: the bound must have room for a future
    // master a little larger than today's biggest, so adding one does not
    // silently need the validation bound widened. The largest shipped
    // master sits at or below four fifths of the bound.
    assert!(
        largest * 5 <= MAX_WALLPAPER_BYTES * 4,
        "the largest shipped master is {largest} bytes, leaving too little \
         headroom under the {MAX_WALLPAPER_BYTES}-byte bound"
    );
}

#[test]
fn the_default_wallpaper_is_one_of_the_shipped_masters() {
    let masters = shipped_masters();
    assert!(
        masters.iter().any(|(name, _)| name == DEFAULT_WALLPAPER),
        "the default wallpaper {DEFAULT_WALLPAPER} is not among the shipped \
         masters {masters:?}"
    );
}

#[test]
fn accepts_every_permitted_extension() {
    let entries = catalog_entries([
        ("a.jpg", 100),
        ("b.jpeg", 100),
        ("c.png", 100),
        ("D.JPG", 100),
        ("E.PnG", 100),
    ]);
    let names: alloc::vec::Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["D.JPG", "E.PnG", "a.jpg", "b.jpeg", "c.png"]);
}

#[test]
fn rejects_files_with_an_unsupported_extension() {
    let entries = catalog_entries([("a.jpg", 100), ("readme.txt", 100), ("b.bmp", 100)]);
    let names: alloc::vec::Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["a.jpg"]);
}

#[test]
fn rejects_names_with_control_characters_or_path_separators() {
    let entries = catalog_entries([
        ("ok.jpg", 100),
        ("has/slash.jpg", 100),
        ("has\u{0}null.jpg", 100),
        ("..", 100),
        (".", 100),
        ("", 100),
    ]);
    let names: alloc::vec::Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["ok.jpg"]);
}

#[test]
fn skips_files_above_the_byte_bound() {
    let entries = catalog_entries([
        ("small.jpg", MAX_WALLPAPER_BYTES),
        ("big.jpg", MAX_WALLPAPER_BYTES + 1),
    ]);
    let names: alloc::vec::Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["small.jpg"], "the exact bound is admitted");
}

#[test]
fn the_result_is_sorted_by_name() {
    let entries = catalog_entries([("z.jpg", 1), ("a.jpg", 1), ("m.png", 1)]);
    let names: alloc::vec::Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["a.jpg", "m.png", "z.jpg"]);
}

#[test]
fn the_result_is_capped_at_the_catalog_bound() {
    let names: alloc::vec::Vec<alloc::string::String> = (0..MAX_WALLPAPER_CATALOG_ENTRIES + 50)
        .map(|i| alloc::format!("wallpaper-{i:04}.jpg"))
        .collect();
    let entries = catalog_entries(names.iter().map(|name| (name.as_str(), 1)));
    assert_eq!(entries.len(), MAX_WALLPAPER_CATALOG_ENTRIES);
    // The retained entries are the lexicographically first ones.
    assert_eq!(entries[0].name, "wallpaper-0000.jpg");
}

#[test]
fn an_empty_listing_yields_an_empty_catalog() {
    let entries = catalog_entries(core::iter::empty::<(&str, usize)>());
    assert!(entries.is_empty());
}

#[test]
fn byte_size_is_carried_through_unchanged() {
    let entries = catalog_entries([("a.jpg", 12_345)]);
    assert_eq!(entries[0].bytes, 12_345);
}
