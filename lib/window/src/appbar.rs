//! The icon-bar declaration most windowed applications make.
//!
//! An application declares its presence on the desktop's icon bar through
//! [`WindowClient::set_app_bar`](crate::WindowClient::set_app_bar), and the
//! declaration is an [`AppBar`]: whether it handles the slot's primary click,
//! and the menu a secondary press opens. Most applications want the same
//! thing there — *Quit*, and the session-drawn *About* row whose panel states
//! the bundle's signed manifest — and want the click left to the session, so
//! it raises their window.
//!
//! That declaration is written once, here, rather than in each application.
//! An application with more to offer (a terminal's *New window*, a viewer's
//! rotations) builds its own [`AppBar`] instead; this is the common case, not
//! a ceiling.

use tairix_abi::window_ipc::{
    AppBar, AppMenu, AppMenuItemId, AppMenuLabel, AppMenuMark, AppMenuRow,
};
use tairix_abi::Errno;

/// The row id the shared declaration gives its *Quit* row.
///
/// An application matches the id it is handed back against this, so the row
/// the bar draws and the command it runs are named by one definition rather
/// than by a literal in each application.
pub const QUIT_ROW: u16 = 1;

/// The label the shared declaration gives its *Quit* row.
const QUIT_LABEL: &str = "Quit";

/// The commonest icon-bar declaration: a *Quit* row, the session-drawn
/// *About* row, and no default action of the application's own.
///
/// `endpoint` is the application's own event mailbox — where the session
/// delivers the chosen row. No default action means a primary click on the
/// slot raises the application's most recently used window, which is what an
/// application whose slot simply fronts its window wants.
///
/// # Errors
///
/// Any [`Errno`] the bounded menu builder refuses. The rows are fixed, so a
/// refusal can only mean the shared bounds changed under this declaration;
/// the caller reports it and runs without a bar rather than showing one it
/// could not describe.
pub fn quit_and_about(endpoint: u64) -> Result<AppBar, Errno> {
    let mut menu = AppMenu::EMPTY;
    menu.push(AppMenuRow::Item {
        id: AppMenuItemId::new(QUIT_ROW)?,
        label: AppMenuLabel::new(QUIT_LABEL)?,
        enabled: true,
        mark: AppMenuMark::None,
    })?;
    menu.push(AppMenuRow::About)?;
    Ok(AppBar {
        event_endpoint: endpoint,
        default_action: false,
        menu,
    })
}

/// Whether `item` is the shared declaration's *Quit* row.
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

    #[test]
    fn the_shared_declaration_offers_quit_and_about_and_no_default_action() {
        let bar = quit_and_about(7).expect("the fixed rows fit");
        assert_eq!(bar.event_endpoint, 7);
        assert!(
            !bar.default_action,
            "the session raises the application's window rather than telling it about the click"
        );
        let rows: alloc::vec::Vec<AppMenuRow> = bar.menu.rows().map(|(row, _)| row).collect();
        assert_eq!(rows.len(), 2);
        assert!(matches!(
            rows[0],
            AppMenuRow::Item { id, enabled: true, .. } if id.get() == QUIT_ROW
        ));
        assert_eq!(rows[1], AppMenuRow::About);
    }

    #[test]
    fn only_the_quit_row_is_recognised() {
        assert!(is_quit(AppMenuItemId::new(QUIT_ROW).expect("non-zero")));
        assert!(!is_quit(
            AppMenuItemId::new(QUIT_ROW + 1).expect("non-zero")
        ));
    }
}
