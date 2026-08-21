//! Host tests for the file manager's icon-bar declaration.
//!
//! What the component's slot offers is a pure function of the places rail, so
//! all of it is exercised here without a kernel: which places become rows, in
//! what order, where the volume rule falls, what the row cap drops, and how a
//! chosen row maps back to the place the user saw.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::window_ipc::{AppMenuItemId, AppMenuRow, APP_MENU_LABEL_MAX, APP_MENU_MAX_ROWS};
use tairix_browse::{Places, Volume};

use super::{component_declaration, place_of, DESKTOP_DEFAULT_ACTION};

/// A home whose three derived places (Home, Desktop, Documents) join the two
/// machine roots, so the fixed rail is five rows long.
fn home() -> Vec<String> {
    alloc::vec!["Users".to_string(), "ada".to_string()]
}

/// A mounted volume at `/Volumes/<label>` with no reported medium.
fn volume(label: &str) -> Volume {
    Volume {
        label: label.to_string(),
        target: alloc::format!("/Volumes/{label}"),
        medium: None,
    }
}

/// The rows a declaration carried, in order.
fn rows(places: &Places) -> Vec<AppMenuRow> {
    let (bar, _) = component_declaration(7, places).expect("the rows fit");
    bar.menu.rows().map(|(row, _)| row).collect()
}

/// The labels of a declaration's item rows, in order.
fn labels(places: &Places) -> Vec<String> {
    rows(places)
        .iter()
        .filter_map(|row| match row {
            AppMenuRow::Item { label, .. } => Some(label.as_str().to_string()),
            _ => None,
        })
        .collect()
}

#[test]
fn the_component_offers_the_places_and_neither_of_the_conventions_rows() {
    let places = Places::new(&home(), &[]);
    let (bar, skipped) = component_declaration(7, &places).expect("the rows fit");
    assert_eq!(bar.event_endpoint, 7);
    assert_eq!(
        bar.default_action, DESKTOP_DEFAULT_ACTION,
        "a click on a component's slot opens a window rather than raising one"
    );
    assert_eq!(skipped, 0);
    // A component states no identity panel of its own and is not the user's to
    // quit, so neither convention row is declared.
    let rows: Vec<AppMenuRow> = bar.menu.rows().map(|(row, _)| row).collect();
    assert!(
        !rows.contains(&AppMenuRow::About),
        "a component declares no information row"
    );
    assert!(
        !labels(&places).iter().any(|label| label == "Quit"),
        "a component declares no Quit row"
    );
    // Every row is the rail's, in the rail's own order.
    assert_eq!(
        labels(&places),
        alloc::vec![
            "Home".to_string(),
            "Desktop".to_string(),
            "Documents".to_string(),
            "Apps".to_string(),
            "System".to_string(),
        ]
    );
    assert!(
        bar.menu.rows().all(|(_, parent)| parent.is_none()),
        "the component declares no submenu"
    );
}

#[test]
fn a_rule_opens_the_mounted_volumes_and_only_when_there_are_some() {
    // Nothing mounted: no divider, because there is nothing to divide.
    let bare = Places::new(&home(), &[]);
    assert!(
        !rows(&bare).contains(&AppMenuRow::Separator),
        "no volumes, no rule"
    );

    // Mounted: the rule falls exactly where the volume rows begin, and the
    // volumes follow the user's own places.
    let mounted = Places::new(&home(), &[volume("Backup"), volume("Stick")]);
    let declared = rows(&mounted);
    let rule = declared
        .iter()
        .position(|row| *row == AppMenuRow::Separator)
        .expect("the rule is declared");
    let volume_start = mounted.volume_start().expect("a volume row exists");
    assert_eq!(
        rule, volume_start,
        "one divider, at the rail's own volume boundary"
    );
    assert_eq!(
        declared
            .iter()
            .filter(|row| **row == AppMenuRow::Separator)
            .count(),
        1,
        "one rule, not one per volume"
    );
    // Sorted by label, after the fixed places.
    assert_eq!(
        labels(&mounted)[5..],
        ["Backup".to_string(), "Stick".to_string()]
    );
}

#[test]
fn a_chosen_row_names_the_place_the_user_saw() {
    let places = Places::new(&home(), &[volume("Backup")]);
    let (bar, _) = component_declaration(7, &places).expect("the rows fit");
    for (row, _) in bar.menu.rows() {
        let AppMenuRow::Item { id, label, .. } = row else {
            continue;
        };
        let index = place_of(id).expect("a declared id names a place");
        assert_eq!(
            places.rows()[index].label(),
            label.as_str(),
            "the row resolves to the place whose label it drew"
        );
    }
    // An id no declaration carried names nothing rather than a guessed place.
    assert_eq!(place_of(AppMenuItemId::new(1).expect("non-zero")), Some(0));
    let past_the_end =
        place_of(AppMenuItemId::new(9999).expect("non-zero")).expect("the id maps to an index");
    assert!(
        places.rows().get(past_the_end).is_none(),
        "an index past the rail is the caller's to reject, and it is out of range"
    );
}

#[test]
fn a_label_the_menus_bounds_refuse_is_skipped_and_counted() {
    // A volume label the rail accepts but a menu row cannot hold: skipped
    // rather than truncated into something that reads like another volume,
    // and counted so the caller can say some are not shown.
    let long = "v".repeat(APP_MENU_LABEL_MAX + 1);
    let places = Places::new(&home(), &[volume(&long), volume("Stick")]);
    let (bar, skipped) = component_declaration(7, &places).expect("the rows fit");
    assert_eq!(skipped, 1);
    let shown: Vec<String> = bar
        .menu
        .rows()
        .filter_map(|(row, _)| match row {
            AppMenuRow::Item { label, .. } => Some(label.as_str().to_string()),
            _ => None,
        })
        .collect();
    assert!(!shown.iter().any(|label| label.starts_with("vv")));
    assert!(shown.contains(&"Stick".to_string()));
}

#[test]
fn the_row_cap_drops_the_tail_and_reports_how_many() {
    // Five fixed places, a rule, and as many volumes as the cap allows; the
    // rest are dropped rather than silently overflowing the declaration.
    let volumes: Vec<Volume> = (0..12)
        .map(|n| volume(&alloc::format!("v{n:02}")))
        .collect();
    let places = Places::new(&home(), &volumes);
    let (bar, skipped) = component_declaration(7, &places).expect("the rows fit");
    assert_eq!(bar.menu.len(), APP_MENU_MAX_ROWS, "the cap is filled");
    assert_eq!(
        skipped,
        places.rows().len() - (APP_MENU_MAX_ROWS - 1),
        "every place past the cap — the rule taking one row of it — is counted"
    );
    // What is shown is still a prefix of the rail, so the rows the user sees
    // are the ones the rail would have shown first.
    let shown = labels(&places);
    let expected: Vec<String> = places
        .rows()
        .iter()
        .take(shown.len())
        .map(|place| place.label().to_string())
        .collect();
    assert_eq!(shown, expected);
}

#[test]
fn an_empty_rail_declares_an_empty_menu_rather_than_failing() {
    // `Places` always offers the machine roots, so this is the degenerate
    // shape rather than a reachable one — but a menu with no rows is a menu
    // the bar opens nothing for, which is honest, not an error.
    let places = Places::default();
    let (bar, skipped) = component_declaration(7, &places).expect("an empty menu is admissible");
    assert_eq!(bar.menu.len(), 0);
    assert_eq!(skipped, 0);
}
