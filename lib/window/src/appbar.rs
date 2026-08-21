//! The icon-bar declaration most windowed applications make.
//!
//! An application declares its presence on the desktop's icon bar through
//! [`WindowClient::set_app_bar`](crate::WindowClient::set_app_bar), and the
//! declaration is an [`AppBar`]: whether it handles the slot's primary click,
//! and the menu a secondary press opens.
//!
//! # The convention, defined here
//!
//! Every icon-bar menu on this desktop reads the same way: the
//! session-drawn **information** row first, the application's own rows next,
//! then a rule and **Quit** last. That order is a desktop-wide convention, so
//! it is written once — in [`declaration`] — rather than restated by each
//! application (§2.2). An application supplies only the rows in the middle;
//! it cannot place the two ends, so it cannot get them wrong.
//!
//! Most applications have no rows of their own and want the click left to the
//! session so it raises their window: [`info_and_quit`] is that case named.
//!
//! # Components declare differently
//!
//! A bundle the desktop autostarts as a *component of itself* — rather than as
//! an application the user started — makes a different declaration: a
//! permanent slot with the component's own rows, and no *Quit* row at all,
//! because the desktop's own parts are not the user's to close. The session
//! asks for that role with [`DESKTOP_ROLE_SWITCH`] on the command line, which
//! is why the switch is named here beside the declaration it selects: the
//! session that passes it and the bundle that parses it must agree on it
//! exactly, and it is the one thing that tells them apart (§2.2).

use tairix_abi::window_ipc::{
    AppBar, AppMenu, AppMenuItemId, AppMenuLabel, AppMenuMark, AppMenuRow,
};
use tairix_abi::Errno;

/// The row id the convention gives its *Quit* row.
///
/// An application matches the id it is handed back against this, so the row
/// the bar draws and the command it runs are named by one definition rather
/// than by a literal in each application. An application numbering its own
/// rows derives them from this id so the two can never collide.
pub const QUIT_ROW: u16 = 1;

/// The label the convention gives its *Quit* row.
const QUIT_LABEL: &str = "Quit";

/// The command-line switch with which the desktop session asks a bundle to run
/// as a **component of the desktop** rather than as an ordinary application.
///
/// A component holds a permanent icon-bar slot from bring-up, offers its own
/// rows there instead of the convention's two, and is not the user's to quit.
/// Only the session's own bring-up passes it, so a bundle launched from
/// anywhere else — a shell, a folder activation, a library row — is the
/// ordinary application and can never become a second component.
pub const DESKTOP_ROLE_SWITCH: &str = "--desktop";

/// An icon-bar declaration carrying `rows` — the application's own top-level
/// rows, in the order it wants them — between the convention's fixed ends.
///
/// The menu is always the information row, then `rows`, then a rule and
/// *Quit*. `endpoint` is the application's own event mailbox, where the
/// session delivers the chosen row; `default_action` is `true` when a primary
/// click on the slot is the application's to handle and `false` to leave the
/// session raising its most recently used window.
///
/// # Errors
///
/// [`Errno::OutOfRange`] when `rows` holds an [`AppMenuRow::Submenu`]: a
/// submenu's children are parented rows ([`AppMenu::push_under`]) and a flat
/// list cannot express them, so a submenu declared here could only ever draw
/// a parent that opens nothing. An application that needs one builds its own
/// [`AppMenu`].
///
/// Otherwise any [`Errno`] the bounded menu builder refuses — `rows` past the
/// row cap, a label the bounds reject, a repeated id. The caller reports it
/// and runs without a bar rather than showing one it could not describe.
pub fn declaration(
    endpoint: u64,
    default_action: bool,
    rows: &[AppMenuRow],
) -> Result<AppBar, Errno> {
    if rows
        .iter()
        .any(|row| matches!(row, AppMenuRow::Submenu { .. }))
    {
        return Err(Errno::OutOfRange);
    }
    let mut menu = AppMenu::EMPTY;
    menu.push(AppMenuRow::About)?;
    for &row in rows {
        menu.push(row)?;
    }
    menu.push(AppMenuRow::Separator)?;
    menu.push(AppMenuRow::Item {
        id: AppMenuItemId::new(QUIT_ROW)?,
        label: AppMenuLabel::new(QUIT_LABEL)?,
        enabled: true,
        mark: AppMenuMark::None,
    })?;
    Ok(AppBar {
        event_endpoint: endpoint,
        default_action,
        menu,
    })
}

/// The commonest icon-bar declaration: the convention's two rows and nothing
/// else, with no default action of the application's own.
///
/// No default action means a primary click on the slot raises the
/// application's most recently used window, which is what an application
/// whose slot simply fronts its window wants.
///
/// # Errors
///
/// As [`declaration`]. The rows are fixed, so a refusal can only mean the
/// shared bounds changed under this declaration.
pub fn info_and_quit(endpoint: u64) -> Result<AppBar, Errno> {
    declaration(endpoint, false, &[])
}

/// Whether `item` is the convention's *Quit* row.
///
/// An id the declaration never carried names no command and answers `false`
/// (fail closed — a menu outcome is never guessed at).
#[must_use]
pub fn is_quit(item: AppMenuItemId) -> bool {
    item.get() == QUIT_ROW
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An enabled, unmarked row, for the declarations under test.
    fn row(id: u16, label: &str) -> AppMenuRow {
        AppMenuRow::Item {
            id: AppMenuItemId::new(id).expect("non-zero"),
            label: AppMenuLabel::new(label).expect("within bounds"),
            enabled: true,
            mark: AppMenuMark::None,
        }
    }

    #[test]
    fn the_convention_puts_the_information_row_first_and_quit_last() {
        let bar = info_and_quit(7).expect("the fixed rows fit");
        assert_eq!(bar.event_endpoint, 7);
        assert!(
            !bar.default_action,
            "the session raises the application's window rather than telling it about the click"
        );
        let rows: alloc::vec::Vec<AppMenuRow> = bar.menu.rows().map(|(row, _)| row).collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], AppMenuRow::About);
        assert_eq!(rows[1], AppMenuRow::Separator);
        assert!(matches!(
            rows[2],
            AppMenuRow::Item { id, enabled: true, .. } if id.get() == QUIT_ROW
        ));
    }

    #[test]
    fn an_applications_own_rows_land_between_the_conventions_ends() {
        let bar = declaration(9, true, &[row(QUIT_ROW + 1, "New window")])
            .expect("the declared rows fit");
        assert!(bar.default_action);
        let rows: alloc::vec::Vec<AppMenuRow> = bar.menu.rows().map(|(row, _)| row).collect();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0], AppMenuRow::About);
        assert!(matches!(
            rows[1],
            AppMenuRow::Item { id, .. } if id.get() == QUIT_ROW + 1
        ));
        assert_eq!(rows[2], AppMenuRow::Separator);
        assert!(matches!(
            rows[3],
            AppMenuRow::Item { id, .. } if id.get() == QUIT_ROW
        ));
    }

    #[test]
    fn a_submenu_row_is_refused_rather_than_drawn_childless() {
        let submenu = AppMenuRow::Submenu {
            label: AppMenuLabel::new("Profile").expect("within bounds"),
            enabled: true,
        };
        assert_eq!(declaration(9, false, &[submenu]), Err(Errno::OutOfRange));
    }

    #[test]
    fn only_the_quit_row_is_recognised() {
        assert!(is_quit(AppMenuItemId::new(QUIT_ROW).expect("non-zero")));
        assert!(!is_quit(
            AppMenuItemId::new(QUIT_ROW + 1).expect("non-zero")
        ));
    }
}
