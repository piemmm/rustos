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
        "/System/Graphics/Wallpapers/TAIRiX/tairix-dark.jpg"
    );
    assert!(default_wallpaper_path().starts_with(WALLPAPER_STORE));
    assert_eq!(category_path("Space"), "/System/Graphics/Wallpapers/Space");
    assert_eq!(
        wallpaper_path("Space", "low-orbit.jpg"),
        "/System/Graphics/Wallpapers/Space/low-orbit.jpg"
    );
}

/// The shipped categories, read from the crate's own `assets/` directory:
/// every subdirectory name, in name order.
fn shipped_categories() -> alloc::vec::Vec<String> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/assets");
    let mut categories: alloc::vec::Vec<String> = std::fs::read_dir(dir)
        .expect("the shipped assets directory")
        .map(|entry| entry.expect("directory entry"))
        .filter(|entry| entry.path().is_dir())
        .map(|entry| {
            entry
                .file_name()
                .into_string()
                .expect("utf-8 category name")
        })
        .collect();
    categories.sort();
    categories
}

/// The shipped masters, measured from the crate's own `assets/` directory:
/// `(category, name, byte length)` for each file, in category-then-name
/// order.
fn shipped_masters() -> alloc::vec::Vec<(String, String, usize)> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/assets");
    let mut masters: alloc::vec::Vec<(String, String, usize)> = alloc::vec::Vec::new();
    for category in shipped_categories() {
        let entries = std::fs::read_dir(std::path::Path::new(dir).join(&category))
            .expect("a shipped category directory");
        for entry in entries {
            let entry = entry.expect("directory entry");
            let name = entry.file_name().into_string().expect("utf-8 asset name");
            let bytes = usize::try_from(entry.metadata().expect("asset metadata").len())
                .expect("asset size fits usize");
            masters.push((category.clone(), name, bytes));
        }
    }
    masters.sort();
    masters
}

#[test]
fn every_shipped_master_is_filed_under_an_offerable_category() {
    let categories = shipped_categories();
    assert!(
        !categories.is_empty(),
        "the crate ships wallpaper categories"
    );
    for category in &categories {
        assert!(
            is_wallpaper_category_name(category),
            "shipped category {category} is not an offerable category name"
        );
    }
    // Every shipped category must survive the rail it will be listed
    // through: one the chooser would silently drop is a whole shipped
    // folder no user could ever reach.
    let listing = catalog_categories(categories.iter().map(String::as_str));
    assert_eq!(listing, categories);
    // A master directly in the store's root would never be planted, and so
    // never offered: the store's own children are categories alone.
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/assets");
    for entry in std::fs::read_dir(dir).expect("the shipped assets directory") {
        let path = entry.expect("directory entry").path();
        assert!(
            path.is_dir(),
            "{} sits outside a category directory",
            path.display()
        );
    }
}

#[test]
fn every_shipped_master_is_offerable_and_fits_within_the_byte_bound() {
    let masters = shipped_masters();
    assert!(!masters.is_empty(), "the crate ships wallpaper masters");
    for (category, name, bytes) in &masters {
        assert!(
            is_wallpaper_file_name(name),
            "shipped master {category}/{name} is not an offerable wallpaper file name"
        );
        assert!(
            *bytes <= MAX_WALLPAPER_BYTES,
            "shipped master {category}/{name} is {bytes} bytes, over the \
             {MAX_WALLPAPER_BYTES}-byte bound"
        );
    }
    // Every shipped master must survive the catalog its own category will be
    // listed through: a master the desktop would silently drop is a shipped
    // asset no user could ever choose.
    for category in shipped_categories() {
        let in_category: alloc::vec::Vec<(&str, usize)> = masters
            .iter()
            .filter(|(owner, _, _)| *owner == category)
            .map(|(_, name, bytes)| (name.as_str(), *bytes))
            .collect();
        assert!(
            !in_category.is_empty(),
            "shipped category {category} holds no master, so its rail entry \
             would open an empty gallery"
        );
        assert_eq!(
            catalog_entries(in_category.iter().copied()).len(),
            in_category.len()
        );
    }
}

#[test]
fn the_byte_bound_admits_the_largest_shipped_master_with_headroom() {
    let largest = shipped_masters()
        .into_iter()
        .map(|(_, _, bytes)| bytes)
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
        masters
            .iter()
            .any(|(category, name, _)| category == DEFAULT_WALLPAPER_CATEGORY
                && name == DEFAULT_WALLPAPER),
        "the default wallpaper {DEFAULT_WALLPAPER_CATEGORY}/{DEFAULT_WALLPAPER} is \
         not among the shipped masters {masters:?}"
    );
}

#[test]
fn categories_are_sorted_and_illegal_names_are_dropped() {
    let listing = catalog_categories(["Space", "has/slash", "Abstract", "..", ".", "", "TAIRiX"]);
    assert_eq!(listing, ["Abstract", "Space", "TAIRiX"]);
}

#[test]
fn the_category_list_is_capped_at_the_category_bound() {
    let names: alloc::vec::Vec<String> = (0..MAX_WALLPAPER_CATEGORIES + 20)
        .map(|i| alloc::format!("Category-{i:04}"))
        .collect();
    let listing = catalog_categories(names.iter().map(String::as_str));
    assert_eq!(listing.len(), MAX_WALLPAPER_CATEGORIES);
    assert_eq!(listing[0], "Category-0000");
}

#[test]
fn an_empty_store_yields_no_categories() {
    assert!(catalog_categories(core::iter::empty::<&str>()).is_empty());
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
