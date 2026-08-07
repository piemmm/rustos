//! The places / devices rail's input routing: every decision the shortcut
//! column down the leading edge of the file manager's window makes about a
//! pointer or keyboard event.
//!
//! # Why this is its own module
//!
//! The `Run` binary around it is a freestanding program — it only exists when
//! the crate is built for a bare-metal target — so nothing inside it can be
//! reached by a host test. The rail's behaviour is worth testing: which press
//! lands on which row, which key moves the cursor, where the focus goes, and
//! what happens when a place cannot be listed. All of that is a pure function
//! of the event, the rail model, and the browser, so it lives here, compiles
//! on the host, and is covered by the tests beside it.
//!
//! # No I/O, and no reporting
//!
//! Nothing here opens, lists, or writes anything. Reading the mount table and
//! the user's home is the program's job, and so is stating a refusal on the
//! error stream: a navigation the filesystem refuses comes back as
//! [`SidebarOutcome::refused`] — the text to state — so the program keeps the
//! one reporting path it already has rather than growing a second one.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::input::{KeyInput, KeyValue, NamedKeyCode, PointerButtonCode};
use tairix_abi::window_ipc::{PointerAction, WindowEvent};
use tairix_browse::render::{sidebar_index_at, toolbar_command_at};
use tairix_browse::{Browser, DirectorySource, Places, ToolbarCommand, Volume};
use tairix_geometry::{Point, Rect, Scale};
use tairix_theme::Theme;

/// What routing an event to the rail did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SidebarOutcome {
    /// Whether anything the window draws changed, and so owes a re-present.
    pub changed: bool,
    /// The reason a place could not be opened, ready to be stated on the
    /// error stream, or `None` when nothing was refused.
    ///
    /// Carried out rather than written here so the program states it through
    /// the single fail-loud reporting path it already uses for every other
    /// refusal, and so a test can read exactly what a user would be told.
    pub refused: Option<String>,
}

impl SidebarOutcome {
    /// An outcome that owes a repaint (or not) and refused nothing.
    #[must_use]
    pub const fn quiet(changed: bool) -> Self {
        Self {
            changed,
            refused: None,
        }
    }
}

/// The window-local point of a primary-button press, or `None` for any other
/// pointer action.
///
/// The rail's hit-test and the view's own routers below it all resolve a press
/// the same way, so this is the one definition of "a primary press, here"
/// rather than a copy per router.
#[must_use]
pub fn press_point(action: PointerAction, x: u32, y: u32) -> Option<Point> {
    if action != PointerAction::Pressed(PointerButtonCode::Primary) {
        return None;
    }
    Some(Point::new(
        i32::try_from(x).unwrap_or(i32::MAX),
        i32::try_from(y).unwrap_or(i32::MAX),
    ))
}

/// Whether `event` is the user asking the window to re-read what is there:
/// the `F5` key or a press on the toolbar's Refresh command.
///
/// The kernel publishes no mount-change notification, so this gesture — not a
/// poll — is what makes a newly attached volume appear in the rail. `viewport`
/// is the area the toolbar was actually drawn in, so the press is tested
/// against the control the user saw.
pub fn is_refresh_request<S: DirectorySource>(
    browser: &Browser<S>,
    scale: Scale,
    theme: &Theme,
    viewport: Rect,
    event: &WindowEvent,
) -> bool {
    match event {
        WindowEvent::Key {
            key:
                KeyInput::Pressed {
                    key: KeyValue::Named(NamedKeyCode::F5),
                    ..
                },
            ..
        } => true,
        WindowEvent::Pointer { x, y, action, .. } => press_point(*action, *x, *y)
            .and_then(|point| toolbar_command_at(browser, scale, theme, viewport, point))
            .is_some_and(|command| command == ToolbarCommand::Refresh),
        _ => false,
    }
}

/// Rebuild `places` from a freshly read `home` and `volumes`, keeping the
/// keyboard focus and cursor where the user left them.
///
/// A refresh must never move the selection out from under the user, so the
/// interaction state survives the rebuild; a cursor past the end of the new
/// rail is simply not restored, which leaves it at the first row.
pub fn refresh_places(places: &mut Places, home: &[String], volumes: &[Volume]) {
    let focused = places.is_focused();
    let cursor = places.cursor();
    *places = Places::new(home, volumes);
    places.set_focused(focused);
    places.set_cursor(cursor);
}

/// Track the rail's hover highlight for a pointer motion, reporting whether
/// the highlight moved (and so owes a repaint).
///
/// Kept apart from [`apply_event`] because a motion that *leaves* the rail
/// must both clear the highlight and still reach the view below — the bundle
/// drag-out detector depends on seeing every motion.
pub fn track_hover(
    places: &mut Places,
    scale: Scale,
    theme: &Theme,
    window: Rect,
    event: &WindowEvent,
) -> bool {
    let WindowEvent::Pointer {
        x,
        y,
        action: PointerAction::Moved,
        ..
    } = event
    else {
        return false;
    };
    let point = Point::new(
        i32::try_from(*x).unwrap_or(i32::MAX),
        i32::try_from(*y).unwrap_or(i32::MAX),
    );
    let row = sidebar_index_at(window, scale, theme, Some(places), point);
    places.set_hovered(row)
}

/// Route one event to the rail, returning `Some(outcome)` when the rail
/// consumed it and `None` when it did not (the caller then routes the event to
/// the browser view as usual).
///
/// The rail owns a primary press on one of its rows — which focuses the rail,
/// puts its cursor on that row, and navigates — and, only while it holds the
/// keyboard focus, the arrows that move its cursor, the `Enter` that activates
/// it, and the `Escape` that hands focus back. `Tab` moves the focus between
/// the rail and the file view from either side, so every state the row control
/// offers is reachable from the keyboard alone.
pub fn apply_event<S: DirectorySource>(
    browser: &mut Browser<S>,
    places: &mut Places,
    scale: Scale,
    theme: &Theme,
    window: Rect,
    event: &WindowEvent,
) -> Option<SidebarOutcome> {
    match event {
        WindowEvent::Key {
            key: KeyInput::Pressed { key, modifiers },
            ..
        } => {
            if matches!(key, KeyValue::Named(NamedKeyCode::Tab))
                && !modifiers.ctrl
                && !modifiers.alt
            {
                let focused = places.is_focused();
                places.set_focused(!focused);
                return Some(SidebarOutcome::quiet(true));
            }
            if !places.is_focused() {
                return None;
            }
            match key {
                KeyValue::Named(NamedKeyCode::Down) => {
                    Some(SidebarOutcome::quiet(places.move_cursor(1)))
                }
                KeyValue::Named(NamedKeyCode::Up) => {
                    Some(SidebarOutcome::quiet(places.move_cursor(-1)))
                }
                KeyValue::Named(NamedKeyCode::Enter) => {
                    let cursor = places.cursor();
                    Some(navigate_to(browser, places, cursor))
                }
                KeyValue::Named(NamedKeyCode::Escape) => {
                    places.set_focused(false);
                    Some(SidebarOutcome::quiet(true))
                }
                // While the rail holds focus its keys are its own: a keystroke
                // it has no use for is swallowed rather than navigating the
                // listing behind it.
                _ => Some(SidebarOutcome::quiet(false)),
            }
        }
        WindowEvent::Pointer { x, y, action, .. } => {
            let point = press_point(*action, *x, *y)?;
            let index = sidebar_index_at(window, scale, theme, Some(places), point)?;
            places.set_focused(true);
            places.set_cursor(index);
            let mut outcome = navigate_to(browser, places, index);
            // The focus and cursor moved whatever the navigation did, so the
            // press always owes a repaint.
            outcome.changed = true;
            Some(outcome)
        }
        _ => None,
    }
}

/// Navigate the browser to the place at `index`.
///
/// A row the rail has already found unavailable does nothing — a disabled
/// control does not act. A navigation the filesystem refuses leaves the
/// browser exactly where it was, marks the row unavailable so it reads
/// disabled from then on, and returns the reason to state: a place that cannot
/// be listed says so rather than wedging or blanking the window.
pub fn navigate_to<S: DirectorySource>(
    browser: &mut Browser<S>,
    places: &mut Places,
    index: usize,
) -> SidebarOutcome {
    let Some(place) = places.rows().get(index) else {
        return SidebarOutcome::quiet(false);
    };
    if !place.is_available() {
        return SidebarOutcome::quiet(false);
    }
    let label = String::from(place.label());
    let components: Vec<String> = place.components().to_vec();
    let Ok(moved) = browser.navigate_to(components) else {
        places.set_unavailable(index);
        return SidebarOutcome {
            changed: true,
            refused: Some(alloc::format!("could not open {label}")),
        };
    };
    SidebarOutcome::quiet(moved)
}

#[cfg(test)]
#[path = "sidebar_tests.rs"]
mod tests;
