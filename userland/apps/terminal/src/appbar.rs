//! The terminal's icon-bar declaration: what its slot on the desktop's bar
//! offers.
//!
//! The desktop draws the slot and owns the menu's pixels; the terminal only
//! declares what is on it. One row is its own — *New window*, which opens
//! another terminal in the same process — and the shared convention
//! ([`tairix_window::declaration`]) places the session-drawn information row
//! above it and *Quit* below, so this menu reads in the same order as every
//! other application's.
//!
//! A primary click on the slot is the terminal's to handle ([`SLOT_CLICK`]),
//! and means the same as *New window*: the readiest thing a terminal can do
//! for you is give you another one.
//!
//! The row ids live here rather than in the program body because two
//! independent readers need them: the running program, which matches the
//! chosen row back to a command, and the desktop QEMU vertical, which
//! reconstructs the menu to know where to click. Naming them once is what
//! stops the two from drifting.

use tairix_abi::window_ipc::{
    AppBar, AppBarClick, AppMenuItem, AppMenuItemId, AppMenuLabel, AppMenuRow,
};
use tairix_abi::Errno;
use tairix_window::QUIT_ROW;

/// The *New window* row's id: open another terminal window in this process.
///
/// Derived from the convention's own [`QUIT_ROW`] rather than written as a
/// literal, so this terminal's row and the shared *Quit* row can never end up
/// naming the same id.
pub const ROW_NEW_WINDOW: u16 = QUIT_ROW + 1;

/// What a primary click on the terminal's icon-bar slot does.
///
/// Every click opens a fresh window, exactly as the *New window* row does —
/// including when the terminal already has windows. They are
/// interchangeable, so raising one of them is less use than making another,
/// and a terminal left on the bar with none is opened by the same click.
pub const SLOT_CLICK: AppBarClick = AppBarClick::Open;

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
        if tairix_window::is_quit(item) {
            return Some(Self::Quit);
        }
        match item.get() {
            ROW_NEW_WINDOW => Some(Self::NewWindow),
            _ => None,
        }
    }
}

/// The terminal's icon-bar declaration, addressed to its own `endpoint`.
///
/// # Errors
///
/// Any [`Errno`] the shared convention refuses. The rows are fixed, so a
/// refusal can only mean the shared bounds changed under this declaration;
/// the caller reports it and runs without a bar rather than showing one it
/// could not describe.
pub fn declaration(endpoint: u64) -> Result<AppBar, Errno> {
    let new_window = AppMenuRow::Item(AppMenuItem::new(
        AppMenuItemId::new(ROW_NEW_WINDOW)?,
        AppMenuLabel::new("New window")?,
    ));
    tairix_window::declaration(endpoint, SLOT_CLICK, &[new_window])
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_abi::window_ipc::AppMenuRowView;

    #[test]
    fn the_declaration_reads_information_new_window_then_quit() {
        let bar = declaration(42).expect("the fixed rows fit");
        assert_eq!(bar.event_endpoint, 42);
        assert_eq!(bar.click, AppBarClick::Open);
        let rows: alloc::vec::Vec<AppMenuRowView<'_>> =
            bar.menu.rows().map(|(row, _)| row).collect();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0], AppMenuRowView::Info);
        assert!(matches!(
            rows[1],
            AppMenuRowView::Item(item) if item.id.get() == ROW_NEW_WINDOW
        ));
        assert_eq!(rows[2], AppMenuRowView::Separator);
        assert!(matches!(
            rows[3],
            AppMenuRowView::Item(item) if item.id.get() == QUIT_ROW
        ));
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
            BarCommand::from_item(AppMenuItemId::new(QUIT_ROW).expect("non-zero")),
            Some(BarCommand::Quit)
        );
        assert_eq!(
            BarCommand::from_item(AppMenuItemId::new(99).expect("non-zero")),
            None,
            "an id this terminal never declared names no command"
        );
    }
}
