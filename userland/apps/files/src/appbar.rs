//! The file manager's icon-bar declaration: what its slot on the desktop's
//! bar offers in each of its two roles.
//!
//! # Why this is its own module
//!
//! The `Run` binary around it is a freestanding program — it only exists when
//! the crate is built for a bare-metal target — so nothing inside it can be
//! reached by a host test. What the slot offers is worth testing: which places
//! become rows, in what order, which are refused, what happens at the row cap,
//! and how a chosen row maps back to the place it names. All of that is a pure
//! function of the [`Places`] model, so it lives here, compiles on the host,
//! and is covered by the tests beside it.
//!
//! # Two roles, two declarations
//!
//! In the ordinary [`Role::Window`](crate::command::Role::Window) role the app
//! is an application like any other and makes the shared declaration — the
//! information row and *Quit*, the click left to the session so it raises the
//! window.
//!
//! In the [`Role::Desktop`](crate::command::Role::Desktop) role it is a
//! component of the desktop, and the slot is the user's way *into* the
//! filesystem rather than a handle on one window. So it offers neither of the
//! convention's rows — a component states no identity panel of its own and is
//! not the user's to quit — and instead lists the places the browser's own
//! rail lists: the user's home and folders, the machine's roots, then whatever
//! is mounted right now. Choosing one opens a window there, and a primary
//! click on the slot opens one at the user's home
//! ([`DESKTOP_DEFAULT_ACTION`]).
//!
//! # The row cap is a display cap, and it is stated
//!
//! An icon-bar menu holds at most [`APP_MENU_MAX_ROWS`] rows, which the fixed
//! places and a busy machine's volumes can exceed. The rows past the cap are
//! **dropped**, and how many is reported to the caller so it can say so —
//! a truncated list that claims to be the whole one would be worse than a
//! short one that admits it.

use tairix_abi::window_ipc::{
    AppBar, AppMenu, AppMenuItemId, AppMenuLabel, AppMenuMark, AppMenuRow, APP_MENU_MAX_ROWS,
};
use tairix_abi::Errno;
use tairix_browse::{PlaceKind, Places};

/// Whether a primary click on the component's slot is the app's to handle.
///
/// `true`: a click opens a window at the user's home, rather than raising one
/// of the windows already open. A component may hold no window at all, so
/// there is often nothing to raise; and when there is, "another window, where
/// I keep my things" is the readier thing to offer.
pub const DESKTOP_DEFAULT_ACTION: bool = true;

/// The component's declaration for `places`, addressed to its own `endpoint`,
/// together with the number of places that did not fit the row cap.
///
/// One row per place in rail order, with a rule where the user's own places
/// give way to the mounted volumes, so the menu reads as two groups rather
/// than one long list. A row carries the place's own index as its id, so a
/// place that had to be skipped shifts nothing: the caller resolves a chosen
/// row through [`place_of`] and lands on exactly the place the user saw.
///
/// A place whose label the menu's own bounds refuse is skipped rather than
/// truncated — a row reading almost like a volume's name would navigate
/// somewhere the user did not ask for. It is counted in the returned total
/// alongside the ones the cap dropped, so the caller states one honest
/// "and some are not shown".
///
/// # Errors
///
/// Any [`Errno`] the bounded menu builder refuses. Every row is bounded and
/// counted before it is pushed, so a refusal can only mean the shared bounds
/// changed under this declaration; the caller reports it and runs without a
/// bar rather than showing one it could not describe.
pub fn component_declaration(endpoint: u64, places: &Places) -> Result<(AppBar, usize), Errno> {
    let mut menu = AppMenu::EMPTY;
    let mut shown = 0;
    let mut skipped = 0;
    let mut ruled = false;
    for (index, place) in places.rows().iter().enumerate() {
        let Ok(label) = AppMenuLabel::new(place.label()) else {
            skipped += 1;
            continue;
        };
        let Ok(id) = row_id(index) else {
            skipped += 1;
            continue;
        };
        // The rule opens the volume group, before the first volume row that
        // actually makes it in — so a menu whose volumes were all skipped
        // never ends on a dangling divider. It costs a row of the cap, so the
        // pair is admitted together or not at all.
        let rule = !ruled && shown > 0 && place.kind() == PlaceKind::Volume;
        if menu.len() + 1 + usize::from(rule) > APP_MENU_MAX_ROWS {
            skipped += 1;
            continue;
        }
        if rule {
            menu.push(AppMenuRow::Separator)?;
            ruled = true;
        }
        menu.push(AppMenuRow::Item {
            id,
            label,
            enabled: place.is_available(),
            mark: AppMenuMark::None,
        })?;
        shown += 1;
    }
    Ok((
        AppBar {
            event_endpoint: endpoint,
            default_action: DESKTOP_DEFAULT_ACTION,
            menu,
        },
        skipped,
    ))
}

/// The index into [`Places::rows`] that the chosen row `item` names, or `None`
/// for an id no declaration carried (fail closed — a menu outcome is never
/// guessed at). The caller still checks the index against the rail it holds:
/// the places may have been re-read since the menu was declared.
#[must_use]
pub fn place_of(item: AppMenuItemId) -> Option<usize> {
    usize::from(item.get()).checked_sub(1)
}

/// The row id for the place at `index`: its index, shifted past the reserved
/// zero so an id always names exactly one place.
fn row_id(index: usize) -> Result<AppMenuItemId, Errno> {
    let raw = u16::try_from(index.saturating_add(1)).map_err(|_| Errno::OutOfRange)?;
    AppMenuItemId::new(raw)
}

#[cfg(test)]
#[path = "appbar_tests.rs"]
mod tests;
