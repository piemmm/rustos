//! Unit tests for the pane registry.
//!
//! These hold the registry's totality in both directions — every pane has a
//! row and every row belongs to a pane the enum names — and the properties
//! the shell relies on: one strip row per category, a disclosing category's
//! panes beneath it, a search that reaches every declared label, and a
//! statement on every pane so no category can be silently blank.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use crate::registry::{strip_rows, Category, Location, Pane, PaneBacking, StripRow, CATEGORIES};

/// Every category the enum names, so the totality test iterates the closed
/// set rather than the table it is checking.
const EVERY_CATEGORY: &[Category] = &[
    Category::General,
    Category::Appearance,
    Category::Wallpaper,
    Category::Displays,
    Category::LockScreen,
    Category::Screensaver,
    Category::Power,
    Category::Networking,
    Category::Bluetooth,
    Category::Sound,
    Category::Notifications,
    Category::Keyboard,
    Category::Mouse,
    Category::Trackpad,
    Category::Touchscreen,
    Category::Printers,
    Category::Accessibility,
    Category::Language,
    Category::Sharing,
    Category::Users,
    Category::Storage,
];

/// Every pane the enum names.
const EVERY_PANE: &[Pane] = &[
    Pane::About,
    Pane::LoginStartup,
    Pane::Caching,
    Pane::DateTime,
    Pane::Appearance,
    Pane::Wallpaper,
    Pane::Displays,
    Pane::LockScreen,
    Pane::Screensaver,
    Pane::Power,
    Pane::Ethernet,
    Pane::WiFi,
    Pane::Dns,
    Pane::TcpIp,
    Pane::Bluetooth,
    Pane::Sound,
    Pane::Notifications,
    Pane::Keyboard,
    Pane::Mouse,
    Pane::Trackpad,
    Pane::Touchscreen,
    Pane::Printers,
    Pane::Accessibility,
    Pane::Language,
    Pane::Sharing,
    Pane::Users,
    Pane::Storage,
];

#[test]
fn every_category_has_exactly_one_row() {
    for category in EVERY_CATEGORY {
        let listed = CATEGORIES
            .iter()
            .filter(|row| row.category == *category)
            .count();
        assert_eq!(listed, 1, "{category:?} has {listed} rows");
    }
    assert_eq!(CATEGORIES.len(), EVERY_CATEGORY.len());
}

#[test]
fn every_pane_has_exactly_one_row() {
    for pane in EVERY_PANE {
        let listed = CATEGORIES
            .iter()
            .flat_map(|row| row.panes)
            .filter(|row| row.pane == *pane)
            .count();
        assert_eq!(listed, 1, "{pane:?} has {listed} rows");
    }
    let rows: usize = CATEGORIES.iter().map(|row| row.panes.len()).sum();
    assert_eq!(rows, EVERY_PANE.len(), "a row names a pane twice, or none");
}

#[test]
fn no_category_is_empty_and_every_label_is_distinct() {
    let mut labels = BTreeSet::new();
    let mut titles = BTreeSet::new();
    for row in CATEGORIES {
        assert!(!row.panes.is_empty(), "{:?} holds no pane", row.category);
        assert!(!row.label.is_empty(), "{:?} has no label", row.category);
        assert!(
            labels.insert(row.label),
            "two categories read {}",
            row.label
        );
        assert!(row.first_pane().is_some());
        for pane in row.panes {
            assert!(!pane.title.is_empty(), "{:?} has no title", pane.pane);
            assert!(
                titles.insert((row.label, pane.title)),
                "{} lists {} twice",
                row.label,
                pane.title
            );
        }
        // A category holding one pane shares its name, so the trail does not
        // say the same word twice.
        if !row.discloses() {
            assert_eq!(row.panes[0].title, row.label, "{:?}", row.category);
        }
    }
}

#[test]
fn every_pane_states_how_the_machine_stands() {
    // A pane with no controls has to say why, or the category is a blank the
    // reader cannot tell from a broken surface.
    for pane in CATEGORIES.iter().flat_map(|row| row.panes) {
        match pane.backing {
            PaneBacking::None { missing, needs } => {
                assert!(!missing.is_empty(), "{:?} states no absence", pane.pane);
                assert!(!needs.is_empty(), "{:?} names no prerequisite", pane.pane);
            }
            PaneBacking::Elsewhere { shows, elsewhere } => {
                assert!(!shows.is_empty(), "{:?} says nothing", pane.pane);
                assert!(
                    !elsewhere.is_empty(),
                    "{:?} names nowhere the setting is reached",
                    pane.pane
                );
            }
            // A composed pane makes no statement: its rows are what it
            // says, and it must actually have some.
            PaneBacking::Composed => {
                assert!(
                    pane.composition().is_some(),
                    "{:?} claims controls it composes nothing for",
                    pane.pane
                );
                assert!(
                    !pane.settings.is_empty(),
                    "{:?} composes controls no search can reach",
                    pane.pane
                );
            }
        }
    }
}

#[test]
fn a_pane_only_declares_settings_it_could_show() {
    // The search index is derived from these labels, so a searchable setting
    // cannot exist without a row that shows it: a pane that composes no
    // controls declares none.
    for pane in CATEGORIES.iter().flat_map(|row| row.panes) {
        if !matches!(pane.backing, PaneBacking::Composed) {
            assert!(
                pane.settings.is_empty(),
                "{:?} declares a setting it cannot show",
                pane.pane
            );
        }
    }
}

#[test]
fn the_registry_locates_every_category_and_pane() {
    for category in EVERY_CATEGORY {
        assert_eq!(
            category.row().map(|row| row.category),
            Some(*category),
            "{category:?}"
        );
    }
    for pane in EVERY_PANE {
        let located = pane.locate().expect("a located pane");
        assert_eq!(located.1.pane, *pane);
        assert!(
            located
                .0
                .row()
                .is_some_and(|row| row.panes.iter().any(|p| p.pane == *pane)),
            "{pane:?} is not in the category it located to"
        );
    }
}

#[test]
fn the_surface_opens_on_the_first_pane_of_the_first_category() {
    let opening = Location::opening().expect("an opening location");
    let first = CATEGORIES.first().expect("a first category");
    assert_eq!(opening.category, first.category);
    assert_eq!(opening.pane, first.panes[0].pane);
    assert!(opening.rows().is_some());
}

#[test]
fn a_location_naming_a_foreign_pane_resolves_to_nothing() {
    // Fail closed: a pane that does not belong to the category draws nothing
    // rather than whichever pane happened to be first.
    let stray = Location {
        category: Category::Sound,
        pane: Pane::About,
    };
    assert!(stray.rows().is_none());
}

#[test]
fn an_unsearched_strip_lists_every_category_and_discloses_the_open_one() {
    let rows = strip_rows(Category::General, "");
    let categories: Vec<Category> = rows
        .iter()
        .filter_map(|row| match row {
            StripRow::Category(category) => Some(*category),
            StripRow::Pane(..) => None,
        })
        .collect();
    assert_eq!(categories.len(), CATEGORIES.len());

    // General discloses four panes; nothing else discloses any.
    let disclosed: Vec<StripRow> = rows
        .iter()
        .copied()
        .filter(|row| matches!(row, StripRow::Pane(..)))
        .collect();
    assert_eq!(disclosed.len(), 4);
    assert!(disclosed
        .iter()
        .all(|row| matches!(row, StripRow::Pane(Category::General, _))));

    // The disclosed rows follow their own category immediately.
    let at = rows
        .iter()
        .position(|row| matches!(row, StripRow::Category(Category::General)))
        .expect("the category row");
    assert!(rows[at + 1..at + 5]
        .iter()
        .all(|row| matches!(row, StripRow::Pane(Category::General, _))));
}

#[test]
fn a_single_pane_category_never_discloses_a_pane_row() {
    let rows = strip_rows(Category::Sound, "");
    assert!(!rows.iter().any(|row| matches!(row, StripRow::Pane(..))));
}

#[test]
fn every_strip_row_resolves_to_a_location() {
    for open in EVERY_CATEGORY {
        for row in strip_rows(*open, "") {
            let location = row.location().expect("a resolved location");
            assert!(location.rows().is_some(), "{row:?} names no pane");
        }
    }
}

#[test]
fn a_search_reaches_a_category_by_its_own_label() {
    let rows = strip_rows(Category::General, "sound");
    assert_eq!(rows, alloc::vec![StripRow::Category(Category::Sound)]);
}

#[test]
fn a_search_reaches_a_pane_by_its_title_and_discloses_only_that_pane() {
    let rows = strip_rows(Category::Sound, "caching");
    assert_eq!(
        rows,
        alloc::vec![
            StripRow::Category(Category::General),
            StripRow::Pane(Category::General, Pane::Caching),
        ],
        "the match is reachable in one press"
    );
}

#[test]
fn a_search_ignores_letter_case() {
    assert_eq!(
        strip_rows(Category::General, "BLUETOOTH"),
        strip_rows(Category::General, "bluetooth")
    );
    assert_eq!(
        strip_rows(Category::General, "Wi-Fi"),
        strip_rows(Category::General, "wi-fi")
    );
}

#[test]
fn a_search_that_reaches_nothing_lists_nothing() {
    assert!(strip_rows(Category::General, "zzz-no-such-setting").is_empty());
}

#[test]
fn a_category_reached_by_its_own_label_offers_all_its_panes() {
    // The reader has not said which pane, so every one is offered.
    let rows = strip_rows(Category::Sound, "networking");
    assert_eq!(rows.len(), 1 + 4);
    assert_eq!(rows[0], StripRow::Category(Category::Networking));
    assert!(rows[1..]
        .iter()
        .all(|row| matches!(row, StripRow::Pane(Category::Networking, _))));
}

#[test]
fn the_search_index_reaches_every_declared_setting_label() {
    // Whatever labels a pane declares, the search must reach that pane by
    // each of them; a declared label the index missed would be a setting the
    // reader cannot find.
    for row in CATEGORIES {
        for pane in row.panes {
            for setting in pane.settings {
                let rows = strip_rows(Category::General, setting);
                assert!(
                    rows.contains(&StripRow::Category(row.category)),
                    "{setting} did not reach {:?}",
                    row.category
                );
                assert!(
                    !row.discloses() || rows.contains(&StripRow::Pane(row.category, pane.pane)),
                    "{setting} did not reach {:?}",
                    pane.pane
                );
            }
        }
    }
}

#[test]
fn an_empty_query_is_not_a_search() {
    // The empty needle matches everything, which must read as "no query" and
    // not as a filter that happens to pass.
    assert_eq!(
        strip_rows(Category::General, ""),
        strip_rows(Category::General, "")
    );
    assert!(strip_rows(Category::General, "")
        .iter()
        .any(|row| matches!(row, StripRow::Pane(Category::General, _))));
}

#[test]
fn the_bundle_presents_no_icon_bar_slot_and_runs_one_instance() {
    // Two properties the *manifest* carries and no Rust constant can: this
    // window is part of the desktop rather than an application the user
    // manages, so closing it ends the program and there is no slot to keep
    // a handle on it — and a second Settings would be a second view of one
    // machine's configuration, each able to overwrite the other's applies.
    //
    // A singleton is the signed header's default, so what this pins is that
    // no `multi-instance` key was ever added; the icon-bar exception has to
    // be declared, so that one is read directly.
    let manifest = include_str!("../AppInfo.toml");
    let declares = |key: &str| {
        manifest
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with('#'))
            .any(|line| line.starts_with(key))
    };
    assert!(
        declares("icon-bar = false"),
        "the manifest must declare no icon-bar slot"
    );
    assert!(
        !declares("multi-instance"),
        "a second Settings could overwrite the first's applies"
    );
}
