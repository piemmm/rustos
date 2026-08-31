//! The terminal's window menu: the rows a secondary press offers, and the
//! keyboard shortcuts that reach the same commands without one.
//!
//! The terminal describes and the desktop decides. This module builds the row
//! model (`plans/NEW-MENUS.md`) and reads a chosen row back; the plates, the
//! placement, the grab and the dismissal are the session's, and the terminal
//! never draws a menu pixel.
//!
//! Rows are built from one ordered [`Command::ALL`] list and read back through
//! [`Command::from_item`] against that same list, so a reordering cannot
//! re-map what a row does. No row declares a submenu or a panel, so the chain
//! this opens is one plate and hangs no window of its own.

use tairix_abi::window_ipc::{
    AppMenu, AppMenuItem, AppMenuItemId, AppMenuLabel, AppMenuRow, AppMenuShortcut,
};
use tairix_abi::Errno;
use tairix_input::{Key, Modifiers};

use crate::APP_NAME;

/// One command the window menu offers.
///
/// Adding a command means adding a variant, its row in [`Command::ALL`], and
/// its arm in each of the label, caption, group and accelerator matches — the
/// compiler then forces every consumer to state what the new command does.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Open the settings sheet.
    Settings,
    /// Draw the screen one step larger.
    Larger,
    /// Draw the screen one step smaller.
    Smaller,
    /// Return the text size to the profile's default.
    ActualSize,
    /// Clear the screen the emulator is showing.
    Clear,
    /// Close the terminal.
    Close,
}

impl Command {
    /// Every command, in the order the menu lists them.
    pub const ALL: [Self; 6] = [
        Self::Settings,
        Self::Larger,
        Self::Smaller,
        Self::ActualSize,
        Self::Clear,
        Self::Close,
    ];

    /// The command the chosen row `item` names, or `None` for an id this
    /// terminal never declared (fail closed — an outcome is never guessed at).
    ///
    /// The inverse of the one-based numbering [`model`] gives each row.
    #[must_use]
    pub fn from_item(item: AppMenuItemId) -> Option<Self> {
        let index = usize::from(item.get().checked_sub(1)?);
        Self::ALL.get(index).copied()
    }

    /// The row label.
    const fn label(self) -> &'static str {
        match self {
            Self::Settings => "Settings…",
            Self::Larger => "Larger text",
            Self::Smaller => "Smaller text",
            Self::ActualSize => "Actual size",
            Self::Clear => "Clear screen",
            Self::Close => "Close",
        }
    }

    /// The accelerator caption the row states, as the user must type it.
    ///
    /// Every caption here is really honoured by
    /// [`accelerator`](Self::accelerator); a row never advertises a key
    /// combination that does nothing.
    const fn shortcut(self) -> &'static str {
        match self {
            Self::Settings => "Ctrl ,",
            Self::Larger => "Ctrl +",
            Self::Smaller => "Ctrl -",
            Self::ActualSize => "Ctrl 0",
            Self::Clear => "Ctrl Shift K",
            Self::Close => "Ctrl Shift W",
        }
    }

    /// Whether this command begins a new group, drawing a divider above it.
    const fn opens_group(self) -> bool {
        matches!(self, Self::Larger | Self::Clear | Self::Close)
    }

    /// The command `key` with `modifiers` held invokes, if any.
    ///
    /// Only combinations a shell would not otherwise receive as a control
    /// byte are claimed, so intercepting one never swallows input a program
    /// was waiting for. A shifted character is matched on the character the
    /// layout actually produced (`+` as well as `=`, `_` as well as `-`), so
    /// the shortcut works whether or not the user needed shift to type it.
    #[must_use]
    pub fn accelerator(key: Key, modifiers: Modifiers) -> Option<Self> {
        if !modifiers.ctrl {
            return None;
        }
        let Key::Char(ch) = key else {
            return None;
        };
        match ch {
            ',' => Some(Self::Settings),
            '+' | '=' => Some(Self::Larger),
            '-' | '_' => Some(Self::Smaller),
            '0' => Some(Self::ActualSize),
            'k' | 'K' if modifiers.shift => Some(Self::Clear),
            'w' | 'W' if modifiers.shift => Some(Self::Close),
            _ => None,
        }
    }
}

/// The row id for the command at `index` of [`Command::ALL`].
///
/// One-based, because a menu id is never zero; [`Command::from_item`] is the
/// inverse, so the two are one rule rather than two tables to keep in step.
///
/// # Errors
///
/// [`Errno::OutOfRange`] for an index no id can number, which the fixed
/// [`Command::ALL`] cannot reach.
fn row_id(index: usize) -> Result<AppMenuItemId, Errno> {
    let raw = u16::try_from(index)
        .ok()
        .and_then(|position| position.checked_add(1))
        .ok_or(Errno::OutOfRange)?;
    AppMenuItemId::new(raw)
}

/// The menu model a secondary press asks the desktop to open.
///
/// A declared separator is the divider above the row that follows it; the
/// chain folds it into that row's group break rather than drawing a row of
/// its own.
///
/// # Errors
///
/// Any [`Errno`] the shared bounds refuse. The rows are fixed, so a refusal
/// can only mean those bounds changed under this menu; the caller reports it
/// and opens nothing rather than showing a menu it could not describe.
pub fn model() -> Result<AppMenu, Errno> {
    let mut menu = AppMenu::titled(AppMenuLabel::new(APP_NAME)?);
    for (index, command) in Command::ALL.into_iter().enumerate() {
        if command.opens_group() {
            menu.push(AppMenuRow::Separator)?;
        }
        menu.push(AppMenuRow::Item(
            AppMenuItem::new(row_id(index)?, AppMenuLabel::new(command.label())?)
                .with_shortcut(AppMenuShortcut::new(command.shortcut())?),
        ))?;
    }
    Ok(menu)
}

#[cfg(test)]
#[path = "menu_tests.rs"]
mod tests;
