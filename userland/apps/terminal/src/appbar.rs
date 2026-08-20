//! The terminal's icon-bar declaration: what its slot on the desktop's bar
//! offers.
//!
//! The desktop draws the slot and owns the menu's pixels; the terminal only
//! declares what is on it. Two rows are its own — *New window*, which opens
//! another terminal in the same process, and *Quit*, which ends every one of
//! them — plus the session-drawn *About* row, whose panel states the
//! bundle's **signed** manifest rather than anything this program says about
//! itself.
//!
//! A primary click on the slot is the terminal's to handle
//! ([`DEFAULT_ACTION`]), and means the same as *New window*: the readiest
//! thing a terminal can do for you is give you another one.
//!
//! The row ids live here rather than in the program body because two
//! independent readers need them: the running program, which matches the
//! chosen row back to a command, and the desktop QEMU vertical, which
//! reconstructs the menu to know where to click. Naming them once is what
//! stops the two from drifting.

use tairix_abi::window_ipc::{AppBar, AppMenu, AppMenuItemId, AppMenuLabel, AppMenuRow};
use tairix_abi::Errno;

/// The *New window* row's id: open another terminal window in this process.
pub const ROW_NEW_WINDOW: u16 = 1;

/// The *Quit* row's id: close every window and end the process.
pub const ROW_QUIT: u16 = 2;

/// Whether a primary click on the terminal's icon-bar slot is delivered to
/// the terminal rather than raising a window.
///
/// `true`: a click opens a fresh window, exactly as the *New window* row
/// does. A terminal's windows are interchangeable, so raising one of them is
/// less use than making another.
pub const DEFAULT_ACTION: bool = true;

/// Which command a chosen icon-bar row names.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BarCommand {
    /// Open another terminal window in this process.
    NewWindow,
    /// Close every window and end the process.
    Quit,
}

impl BarCommand {
    /// The command the row `item` names, or `None` for an id this terminal
    /// never declared (fail closed — a menu outcome is never guessed at).
    #[must_use]
    pub fn from_item(item: AppMenuItemId) -> Option<Self> {
        match item.get() {
            ROW_NEW_WINDOW => Some(Self::NewWindow),
            ROW_QUIT => Some(Self::Quit),
            _ => None,
        }
    }
}

/// The terminal's icon-bar declaration, addressed to its own `endpoint`.
///
/// # Errors
///
/// Any [`Errno`] the bounded menu builder refuses. The rows are fixed, so a
/// refusal can only mean the shared bounds changed under this declaration;
/// the caller reports it and runs without a bar rather than showing one it
/// could not describe.
pub fn declaration(endpoint: u64) -> Result<AppBar, Errno> {
    let mut menu = AppMenu::EMPTY;
    menu.push(row(ROW_NEW_WINDOW, "New window")?)?;
    menu.push(AppMenuRow::Separator)?;
    menu.push(row(ROW_QUIT, "Quit")?)?;
    menu.push(AppMenuRow::About)?;
    Ok(AppBar {
        event_endpoint: endpoint,
        default_action: DEFAULT_ACTION,
        menu,
    })
}

/// An enabled, unmarked row.
fn row(id: u16, label: &str) -> Result<AppMenuRow, Errno> {
    Ok(AppMenuRow::Item {
        id: AppMenuItemId::new(id)?,
        label: AppMenuLabel::new(label)?,
        enabled: true,
        mark: tairix_abi::window_ipc::AppMenuMark::None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_declaration_offers_a_new_window_a_quit_and_the_about_row() {
        let bar = declaration(42).expect("the fixed rows fit");
        assert_eq!(bar.event_endpoint, 42);
        assert!(bar.default_action);
        let rows: alloc::vec::Vec<AppMenuRow> = bar.menu.rows().map(|(row, _)| row).collect();
        assert_eq!(rows.len(), 4);
        assert!(matches!(
            rows[0],
            AppMenuRow::Item { id, .. } if id.get() == ROW_NEW_WINDOW
        ));
        assert_eq!(rows[1], AppMenuRow::Separator);
        assert!(matches!(
            rows[2],
            AppMenuRow::Item { id, .. } if id.get() == ROW_QUIT
        ));
        assert_eq!(rows[3], AppMenuRow::About);
        assert!(
            bar.menu.rows().all(|(_, parent)| parent.is_none()),
            "the terminal declares no submenu of its own"
        );
    }

    #[test]
    fn a_chosen_row_maps_to_its_command_and_an_unknown_id_to_nothing() {
        assert_eq!(
            BarCommand::from_item(AppMenuItemId::new(ROW_NEW_WINDOW).expect("non-zero")),
            Some(BarCommand::NewWindow)
        );
        assert_eq!(
            BarCommand::from_item(AppMenuItemId::new(ROW_QUIT).expect("non-zero")),
            Some(BarCommand::Quit)
        );
        assert_eq!(
            BarCommand::from_item(AppMenuItemId::new(99).expect("non-zero")),
            None,
            "an id this terminal never declared names no command"
        );
    }
}
