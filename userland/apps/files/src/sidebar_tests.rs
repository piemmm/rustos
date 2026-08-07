//! Host tests for the places / devices rail's input routing.
//!
//! The rail is driven with the same `WindowEvent`s the desktop session
//! delivers, over an in-memory directory source, so every decision the running
//! app makes — which press lands on which row, where the keyboard focus goes,
//! and what a refused place does — is exercised without a kernel.

use alloc::collections::BTreeSet;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::input::{KeyInput, KeyValue, Modifiers, NamedKeyCode, PointerButtonCode};
use tairix_abi::window_ipc::{PointerAction, WindowEvent};
use tairix_abi::Errno;
use tairix_browse::render::{content_area, sidebar_view, toolbar_command_at};
use tairix_browse::{Browser, DirectorySource, Entry, Places, SidebarView, ToolbarCommand, Volume};
use tairix_geometry::{Point, Rect, Scale};
use tairix_theme::Theme;

use super::{apply_event, is_refresh_request, press_point, refresh_places, track_hover};

/// The window the rail is laid out in for these tests.
const WINDOW: Rect = Rect::new(0, 0, 480, 480);

/// The window this app is given, addressed by every synthesised event.
const WINDOW_ID: u64 = 7;

/// An in-memory directory tree: a path either lists (empty) or is refused.
///
/// The rail only cares whether a place *can* be listed, so the listings
/// themselves are empty — what is exercised here is where the browser ends up
/// and what happens when it cannot get there.
struct FakeFs {
    listable: BTreeSet<String>,
}

impl FakeFs {
    /// The user's home tree and the machine roots, with the user's Desktop
    /// deliberately unreadable so the refusal path can be driven.
    fn fixture() -> Self {
        let mut listable = BTreeSet::new();
        for path in [
            "/",
            "/Users",
            "/Users/ann",
            "/Users/ann/Documents",
            "/Apps",
            "/System",
            "/Storage/Backup",
        ] {
            listable.insert(path.to_string());
        }
        Self { listable }
    }
}

impl DirectorySource for FakeFs {
    fn list(&mut self, components: &[String]) -> Result<Vec<Entry>, Errno> {
        let mut path = String::new();
        for component in components {
            path.push('/');
            path.push_str(component);
        }
        if path.is_empty() {
            path.push('/');
        }
        if self.listable.contains(&path) {
            Ok(Vec::new())
        } else {
            Err(Errno::PermissionDenied)
        }
    }
}

/// A browser opened at the storage root.
fn browser() -> Browser<FakeFs> {
    Browser::open_root(FakeFs::fixture()).expect("the fixture root lists")
}

/// The user's home directory components.
fn home() -> Vec<String> {
    vec!["Users".to_string(), "ann".to_string()]
}

/// The rail the tests drive: the user's places plus one mounted volume.
fn places() -> Places {
    Places::new(
        &home(),
        &[Volume {
            label: "Backup".to_string(),
            target: "/Storage/Backup".to_string(),
            medium: Some(BlkDeviceClass::Rotational),
        }],
    )
}

/// The centre of the rail row at `index`.
fn row_centre(places: &Places, index: usize) -> Point {
    let view = rail(places);
    let rect = view
        .row_rect(index)
        .expect("every row of this rail is drawn in a 480px-tall window");
    Point::new(
        rect.origin.x + i32::try_from(rect.width / 2).unwrap_or(0),
        rect.origin.y + i32::try_from(rect.height / 2).unwrap_or(0),
    )
}

/// The rail's geometry in this test's window.
fn rail(places: &Places) -> SidebarView {
    sidebar_view(WINDOW, Scale::ONE, &Theme::dark(), Some(places)).expect("the rail has rows")
}

/// A primary press at `point`.
fn press(point: Point) -> WindowEvent {
    WindowEvent::Pointer {
        window_id: WINDOW_ID,
        x: u32::try_from(point.x).unwrap_or(0),
        y: u32::try_from(point.y).unwrap_or(0),
        action: PointerAction::Pressed(PointerButtonCode::Primary),
    }
}

/// A pointer motion to `point`.
fn motion(point: Point) -> WindowEvent {
    WindowEvent::Pointer {
        window_id: WINDOW_ID,
        x: u32::try_from(point.x).unwrap_or(0),
        y: u32::try_from(point.y).unwrap_or(0),
        action: PointerAction::Moved,
    }
}

/// A key press with no modifiers held.
fn key(key: KeyValue) -> WindowEvent {
    WindowEvent::Key {
        window_id: WINDOW_ID,
        key: KeyInput::Pressed {
            key,
            modifiers: Modifiers::default(),
        },
    }
}

/// A named key press with no modifiers held.
fn named(code: NamedKeyCode) -> WindowEvent {
    key(KeyValue::Named(code))
}

/// Route `event` to the rail with this test's window and theme.
fn route(
    browser: &mut Browser<FakeFs>,
    places: &mut Places,
    event: &WindowEvent,
) -> Option<super::SidebarOutcome> {
    apply_event(browser, places, Scale::ONE, &Theme::dark(), WINDOW, event)
}

#[test]
fn a_press_on_a_row_focuses_the_rail_and_navigates_to_that_place() {
    let mut browser = browser();
    let mut places = places();
    let apps = places
        .index_of(&["Apps".to_string()])
        .expect("the application root is a fixed row");

    let on_apps = press(row_centre(&places, apps));
    let outcome = route(&mut browser, &mut places, &on_apps).expect("the rail owns its own press");

    assert!(outcome.changed);
    assert_eq!(outcome.refused, None);
    assert_eq!(browser.components(), ["Apps".to_string()]);
    assert!(places.is_focused());
    assert_eq!(places.cursor(), apps);
    // The rail shows where the browser now is.
    assert_eq!(places.index_of(browser.components()), Some(apps));
}

#[test]
fn a_press_beyond_the_rail_is_left_to_the_view_below() {
    let mut browser = browser();
    let mut places = places();
    let just_outside = Point::new(i32::try_from(rail(&places).width()).unwrap_or(0), 4);

    assert!(route(&mut browser, &mut places, &press(just_outside)).is_none());
    // Nothing about the rail moved, and the browser stayed put.
    assert!(!places.is_focused());
    assert!(browser.components().is_empty());
}

#[test]
fn tab_moves_the_focus_between_the_rail_and_the_view_in_both_directions() {
    let mut browser = browser();
    let mut places = places();

    // Into the rail…
    let outcome = route(&mut browser, &mut places, &named(NamedKeyCode::Tab));
    assert_eq!(outcome, Some(super::SidebarOutcome::quiet(true)));
    assert!(places.is_focused());

    // …and back out again, from the same key.
    let outcome = route(&mut browser, &mut places, &named(NamedKeyCode::Tab));
    assert_eq!(outcome, Some(super::SidebarOutcome::quiet(true)));
    assert!(!places.is_focused());

    // Escape hands the focus back too, and only while the rail holds it.
    assert!(route(&mut browser, &mut places, &named(NamedKeyCode::Escape)).is_none());
    places.set_focused(true);
    let outcome = route(&mut browser, &mut places, &named(NamedKeyCode::Escape));
    assert_eq!(outcome, Some(super::SidebarOutcome::quiet(true)));
    assert!(!places.is_focused());
}

#[test]
fn the_arrows_walk_the_rail_and_enter_navigates_to_the_cursor() {
    let mut browser = browser();
    let mut places = places();
    places.set_focused(true);

    // Down to Documents (Home, Desktop, Documents).
    assert_eq!(
        route(&mut browser, &mut places, &named(NamedKeyCode::Down)),
        Some(super::SidebarOutcome::quiet(true))
    );
    assert_eq!(
        route(&mut browser, &mut places, &named(NamedKeyCode::Down)),
        Some(super::SidebarOutcome::quiet(true))
    );
    assert_eq!(places.cursor(), 2);

    let outcome = route(&mut browser, &mut places, &named(NamedKeyCode::Enter));
    assert_eq!(outcome, Some(super::SidebarOutcome::quiet(true)));
    assert_eq!(
        browser.components(),
        [
            "Users".to_string(),
            "ann".to_string(),
            "Documents".to_string()
        ]
    );

    // Up walks back and clamps at the first row rather than wrapping.
    route(&mut browser, &mut places, &named(NamedKeyCode::Up));
    route(&mut browser, &mut places, &named(NamedKeyCode::Up));
    assert_eq!(places.cursor(), 0);
    assert_eq!(
        route(&mut browser, &mut places, &named(NamedKeyCode::Up)),
        Some(super::SidebarOutcome::quiet(false))
    );

    // The last row is reachable and clamps at the other end.
    for _ in 0..places.len() + 2 {
        route(&mut browser, &mut places, &named(NamedKeyCode::Down));
    }
    assert_eq!(places.cursor(), places.len() - 1);
    let outcome = route(&mut browser, &mut places, &named(NamedKeyCode::Enter));
    assert_eq!(outcome, Some(super::SidebarOutcome::quiet(true)));
    assert_eq!(
        browser.components(),
        ["Storage".to_string(), "Backup".to_string()]
    );
}

#[test]
fn the_rail_takes_only_the_keys_it_owns_and_only_while_it_is_focused() {
    let mut browser = browser();
    let mut places = places();

    // Unfocused, its arrows belong to the listing.
    assert!(route(&mut browser, &mut places, &named(NamedKeyCode::Down)).is_none());
    assert!(route(&mut browser, &mut places, &named(NamedKeyCode::Enter)).is_none());

    // Focused, a key it has no use for is swallowed rather than reaching the
    // listing behind it.
    places.set_focused(true);
    assert_eq!(
        route(&mut browser, &mut places, &key(KeyValue::Char('x'))),
        Some(super::SidebarOutcome::quiet(false))
    );
    // A key *release* is never the rail's, focused or not.
    let release = WindowEvent::Key {
        window_id: WINDOW_ID,
        key: KeyInput::Released {
            key: KeyValue::Named(NamedKeyCode::Down),
            modifiers: Modifiers::default(),
        },
    };
    assert!(route(&mut browser, &mut places, &release).is_none());
    // And a `Ctrl+Tab` is a shortcut for the window, not a focus move.
    let ctrl_tab = WindowEvent::Key {
        window_id: WINDOW_ID,
        key: KeyInput::Pressed {
            key: KeyValue::Named(NamedKeyCode::Tab),
            modifiers: Modifiers {
                ctrl: true,
                ..Modifiers::default()
            },
        },
    };
    assert_eq!(
        route(&mut browser, &mut places, &ctrl_tab),
        Some(super::SidebarOutcome::quiet(false))
    );
    assert!(places.is_focused());
}

#[test]
fn a_place_that_cannot_be_listed_reports_and_leaves_the_browser_where_it_was() {
    let mut browser = browser();
    let mut places = places();
    let desktop = places
        .index_of(&[
            "Users".to_string(),
            "ann".to_string(),
            "Desktop".to_string(),
        ])
        .expect("Desktop is a fixed row");
    let before = browser.components().to_vec();
    let on_desktop = press(row_centre(&places, desktop));

    let outcome =
        route(&mut browser, &mut places, &on_desktop).expect("the rail owns its own press");

    // Stated, not silent — and naming the place the user clicked.
    assert_eq!(outcome.refused.as_deref(), Some("could not open Desktop"));
    assert!(outcome.changed);
    // The browser did not move, and the row now reads disabled.
    assert_eq!(browser.components(), before.as_slice());
    assert!(!places.rows()[desktop].is_available());

    // A disabled row does not act: activating it again refuses nothing,
    // repaints nothing, and still leaves the browser alone.
    places.set_cursor(desktop);
    places.set_focused(true);
    let again = route(&mut browser, &mut places, &named(NamedKeyCode::Enter));
    assert_eq!(again, Some(super::SidebarOutcome::quiet(false)));
    assert_eq!(browser.components(), before.as_slice());
}

#[test]
fn navigating_to_the_place_already_shown_moves_nothing_but_the_focus() {
    let mut browser = browser();
    let mut places = places();
    let apps = places
        .index_of(&["Apps".to_string()])
        .expect("the application root is a fixed row");
    let on_apps = press(row_centre(&places, apps));
    route(&mut browser, &mut places, &on_apps);

    // Pressing the row the browser is already on repaints (the focus and
    // cursor moved) but refuses nothing and does not re-list.
    let outcome = route(&mut browser, &mut places, &on_apps);
    assert_eq!(outcome, Some(super::SidebarOutcome::quiet(true)));
    assert_eq!(browser.components(), ["Apps".to_string()]);
}

#[test]
fn the_hover_highlight_follows_the_pointer_and_clears_off_the_rail() {
    let mut places = places();
    let theme = Theme::dark();
    let width = rail(&places).width();

    let over_second = motion(row_centre(&places, 1));
    assert!(track_hover(
        &mut places,
        Scale::ONE,
        &theme,
        WINDOW,
        &over_second
    ));
    assert_eq!(places.hovered(), Some(1));
    // The same row again is not a change, so it owes no repaint.
    assert!(!track_hover(
        &mut places,
        Scale::ONE,
        &theme,
        WINDOW,
        &over_second
    ));

    // Leaving the rail clears the highlight.
    let outside = Point::new(i32::try_from(width).unwrap_or(0) + 20, 4);
    assert!(track_hover(
        &mut places,
        Scale::ONE,
        &theme,
        WINDOW,
        &motion(outside)
    ));
    assert_eq!(places.hovered(), None);

    // A press is not a motion: the highlight is only ever moved by one.
    let on_first = press(row_centre(&places, 0));
    assert!(!track_hover(
        &mut places,
        Scale::ONE,
        &theme,
        WINDOW,
        &on_first
    ));
}

#[test]
fn the_refresh_gesture_is_f5_or_the_toolbars_refresh_command() {
    let browser = browser();
    let places = places();
    let theme = Theme::dark();
    let viewport = content_area(WINDOW, Scale::ONE, &theme, Some(&places));

    assert!(is_refresh_request(
        &browser,
        Scale::ONE,
        &theme,
        viewport,
        &named(NamedKeyCode::F5)
    ));
    assert!(!is_refresh_request(
        &browser,
        Scale::ONE,
        &theme,
        viewport,
        &named(NamedKeyCode::Enter)
    ));

    // A press on the toolbar's Refresh control is the same request. The
    // control's position is read from the same layout the toolbar was drawn
    // with, so the test cannot drift from the painted chrome.
    let mut refresh = None;
    let mut other = None;
    let right = viewport.origin.x + i32::try_from(viewport.width).unwrap_or(0);
    let bottom = viewport.origin.y + i32::try_from(viewport.height).unwrap_or(0);
    for y in viewport.origin.y..bottom {
        for x in viewport.origin.x..right {
            let point = Point::new(x, y);
            match toolbar_command_at(&browser, Scale::ONE, &theme, viewport, point) {
                Some(ToolbarCommand::Refresh) if refresh.is_none() => refresh = Some(point),
                Some(command) if command != ToolbarCommand::Refresh && other.is_none() => {
                    other = Some(point);
                }
                _ => {}
            }
        }
        if refresh.is_some() && other.is_some() {
            break;
        }
    }
    let refresh = refresh.expect("the toolbar carries a Refresh command");
    let other = other.expect("the toolbar carries more than one enabled command");
    assert!(is_refresh_request(
        &browser,
        Scale::ONE,
        &theme,
        viewport,
        &press(refresh)
    ));
    assert!(!is_refresh_request(
        &browser,
        Scale::ONE,
        &theme,
        viewport,
        &press(other)
    ));
    // A motion over Refresh is not a request; only a press is.
    assert!(!is_refresh_request(
        &browser,
        Scale::ONE,
        &theme,
        viewport,
        &motion(refresh)
    ));
}

#[test]
fn a_refresh_re_reads_the_volumes_and_keeps_the_users_place_in_the_rail() {
    let mut places = places();
    places.set_focused(true);
    places.set_cursor(2);

    refresh_places(
        &mut places,
        &home(),
        &[
            Volume {
                label: "Backup".to_string(),
                target: "/Storage/Backup".to_string(),
                medium: Some(BlkDeviceClass::Rotational),
            },
            // A stick the user just plugged in: it appears without a poll.
            Volume {
                label: "Stick".to_string(),
                target: "/Storage/Stick".to_string(),
                medium: Some(BlkDeviceClass::Removable),
            },
        ],
    );

    assert!(places.is_focused());
    assert_eq!(places.cursor(), 2);
    assert_eq!(places.len(), 7);
    assert_eq!(places.rows()[6].label(), "Stick");

    // A cursor the shorter rail no longer has is left at the first row rather
    // than pointing past the end.
    places.set_cursor(6);
    refresh_places(&mut places, &home(), &[]);
    assert_eq!(places.len(), 5);
    assert_eq!(places.cursor(), 0);
}

#[test]
fn only_a_primary_press_resolves_to_a_point() {
    assert_eq!(
        press_point(PointerAction::Pressed(PointerButtonCode::Primary), 3, 9),
        Some(Point::new(3, 9))
    );
    assert_eq!(
        press_point(PointerAction::Pressed(PointerButtonCode::Secondary), 3, 9),
        None
    );
    assert_eq!(press_point(PointerAction::Moved, 3, 9), None);
    assert_eq!(
        press_point(PointerAction::Released(PointerButtonCode::Primary), 3, 9),
        None
    );
    // A coordinate no window can hold is clamped rather than wrapped into a
    // negative position that would hit-test somewhere else entirely.
    assert_eq!(
        press_point(
            PointerAction::Pressed(PointerButtonCode::Primary),
            u32::MAX,
            0
        ),
        Some(Point::new(i32::MAX, 0))
    );
}
