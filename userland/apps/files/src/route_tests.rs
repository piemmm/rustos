//! Unit tests for the window-addressing decision.

use crate::route::{addressee, Addressed};

/// Two windows, the second holding a popup.
const WINDOWS: [(u64, Option<u64>); 2] = [(1, None), (2, Some(7))];

#[test]
fn a_windows_own_id_resolves_to_that_window() {
    assert_eq!(addressee(WINDOWS, 1), Some((0, Addressed::Window)));
    assert_eq!(addressee(WINDOWS, 2), Some((1, Addressed::Window)));
}

/// The regression this module exists for: an id matched against the window
/// list alone resolved to nothing, so every key and click delivered to the
/// "Open With…" chooser was dropped and the popup sat on screen inert.
#[test]
fn a_held_popups_id_resolves_to_the_window_that_holds_it() {
    assert_eq!(addressee(WINDOWS, 7), Some((1, Addressed::Popup)));
}

/// And the opposite failure: a popup's event must never be mistaken for its
/// owner's, or a close, resize, or content release aimed at the popup would
/// act on the window behind it.
#[test]
fn a_popups_event_is_never_reported_as_its_owners() {
    let (index, surface) = addressee(WINDOWS, 7).expect("the popup resolves");
    assert_eq!(index, 1, "it resolves to the window that holds it");
    assert_ne!(surface, Addressed::Window, "but not as that window's own");
}

#[test]
fn an_id_no_window_or_popup_carries_resolves_to_nothing() {
    // A window that has just closed: its events have nowhere to land.
    assert_eq!(addressee(WINDOWS, 99), None);
    assert_eq!(addressee([], 1), None);
    // A window with no popup open claims no popup id.
    assert_eq!(addressee([(1u64, None)], 7), None);
}

/// A window's own id wins over any popup claiming it, so a live window's
/// events can never be diverted to an overlay.
#[test]
fn a_windows_own_id_is_matched_before_any_popups() {
    let shadowed = [(9u64, None), (3, Some(9))];
    assert_eq!(addressee(shadowed, 9), Some((0, Addressed::Window)));
}

/// The section strip answers from every section; everything else belongs to
/// the attributes section alone.
///
/// Regression: sectioning the window left the attribute keyboard live on every
/// section, so typing on General filled a field the user could not see and an
/// arrow moved an invisible cursor.
#[test]
fn a_key_reaches_only_the_section_that_draws_its_control() {
    use crate::route::{properties_key, PropertiesKey};
    use tairix_abi::input::{KeyValue, NamedKeyCode};
    use tairix_browse::render::PropertiesTab;

    let named = |code| KeyValue::Named(code);
    let typed = KeyValue::Char('a');

    for tab in PropertiesTab::ALL {
        // The strip is the window's own navigation.
        assert_eq!(
            properties_key(tab, named(NamedKeyCode::Left)),
            PropertiesKey::Section(-1)
        );
        assert_eq!(
            properties_key(tab, named(NamedKeyCode::Right)),
            PropertiesKey::Section(1)
        );
    }

    // On the attributes section its list and editor take the rest.
    for key in [named(NamedKeyCode::Up), named(NamedKeyCode::Down), typed] {
        assert_eq!(
            properties_key(PropertiesTab::Attributes, key),
            PropertiesKey::Attributes
        );
    }
    assert_eq!(
        properties_key(PropertiesTab::Attributes, named(NamedKeyCode::Escape)),
        PropertiesKey::Attributes,
        "the section steps back out of its own typed line before the window closes"
    );

    // On any other section they reach nothing, and Escape closes the window.
    for tab in [PropertiesTab::General, PropertiesTab::Permissions] {
        for key in [named(NamedKeyCode::Up), named(NamedKeyCode::Down), typed] {
            assert_eq!(properties_key(tab, key), PropertiesKey::Ignored);
        }
        assert_eq!(
            properties_key(tab, named(NamedKeyCode::Escape)),
            PropertiesKey::Close
        );
    }
}
