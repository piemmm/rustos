//! The terminal's right-click context menu: the commands it offers, the
//! keyboard shortcuts that reach the same commands without it, and the popup
//! itself.
//!
//! The menu is presentation over a typed [`Command`]: choosing a row only
//! *reports* which command was chosen, and the program carries it out. The
//! rows are built from one ordered [`Command::ALL`] list, and an activated
//! row is read back through that same list, so a reordering cannot silently
//! re-map what a row does.
//!
//! The popup is the shared `lib/controls` [`Menu`] placed by that control's
//! own anchoring rule, so the terminal's menu appears and behaves exactly
//! like every other context menu on the desktop.

use alloc::vec::Vec;

use tairix_controls::{Menu, MenuAction, MenuItem};
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey};
use tairix_raster::Surface;
use tairix_theme::Theme;

/// One command the context menu offers.
///
/// Adding a command means adding a variant, its row in [`Command::ALL`], and
/// its arms in [`Command::label`] / [`Command::shortcut`] — the compiler then
/// forces every consumer to state what the new command does.
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

    /// The row label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Settings => "Settings…",
            Self::Larger => "Larger text",
            Self::Smaller => "Smaller text",
            Self::ActualSize => "Actual size",
            Self::Clear => "Clear screen",
            Self::Close => "Close",
        }
    }

    /// The keyboard shortcut shown on the row, as the user must type it.
    ///
    /// Every shortcut listed here is really honoured by
    /// [`accelerator`](Self::accelerator); a row never advertises a key
    /// combination that does nothing.
    #[must_use]
    pub const fn shortcut(self) -> &'static str {
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

/// What routing one input event into an open menu concluded.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MenuOutcome {
    /// Claimed with no state change.
    Ignored,
    /// Claimed; only pixels changed (a hover or keyboard highlight moved).
    Changed,
    /// A row was chosen; the menu should close and the command run.
    Chose(Command),
    /// The menu was dismissed without choosing.
    Dismissed,
}

/// The open right-click context menu: where it was opened and the rows it
/// shows.
///
/// While one exists it is modal — the program routes every pointer and key
/// event here first, and a click away dismisses it without acting on
/// whatever it landed on.
#[derive(Clone, Debug)]
pub struct ContextMenu {
    /// The window-local point the menu was opened at.
    anchor: Point,
    /// The shared control the rows are drawn and hit-tested through.
    menu: Menu,
    /// The last pointer position seen, so a press can be tested against the
    /// plate: the shared control tracks its own copy but does not publish it.
    pointer: Point,
}

impl ContextMenu {
    /// Open a menu whose top-left starts at window-local `anchor`.
    #[must_use]
    pub fn open(anchor: Point) -> Self {
        let items: Vec<MenuItem> = Command::ALL
            .into_iter()
            .map(|command| {
                MenuItem::new(command.label())
                    .with_shortcut(command.shortcut())
                    .with_group_break(command.opens_group())
            })
            .collect();
        Self {
            anchor,
            menu: Menu::new(items),
            pointer: anchor,
        }
    }

    /// The point the menu was opened at.
    #[must_use]
    pub const fn anchor(&self) -> Point {
        self.anchor
    }

    /// The plate rectangle the menu occupies inside `viewport`.
    #[must_use]
    pub fn bounds(&self, viewport: Rect, scale: Scale, theme: &Theme) -> Rect {
        self.menu.anchored_rect(self.anchor, viewport, scale, theme)
    }

    /// Draw the menu over whatever is already in `surface`.
    pub fn render(&self, surface: &mut Surface, viewport: Rect, scale: Scale, theme: &Theme) {
        let bounds = self.bounds(viewport, scale, theme);
        self.menu.render(surface, bounds, scale, theme);
    }

    /// Route one pointer event.
    ///
    /// A primary press outside the plate dismisses without acting on what it
    /// landed on, so a click meant for the screen behind never runs a command
    /// by accident.
    ///
    /// A sample that leaves the highlight on the row it was already on reports
    /// [`MenuOutcome::Ignored`]: it changes no pixel, and the caller must not
    /// re-render and re-publish the plate for it. Sweeping the pointer across
    /// a row would otherwise cost a frame's work per sample.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> MenuOutcome {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }
        let bounds = self.bounds(viewport, scale, theme);
        if matches!(event, InputEvent::PointerPressed { .. }) && !bounds.contains(self.pointer) {
            return MenuOutcome::Dismissed;
        }
        let action = self.menu.on_pointer(event, bounds, scale, theme, damage);
        Self::outcome(action, damage)
    }

    /// Route one key press.
    ///
    /// A row-highlight move (Up/Down/Home/End) reports [`MenuOutcome::Changed`]
    /// just as a pointer hover does in [`Self::on_pointer`], so the caller
    /// repaints and the moved highlight is actually shown. A key that moves it
    /// nowhere — Down on the last row of a menu that does not wrap, a key the
    /// menu has no use for — reports [`MenuOutcome::Ignored`].
    pub fn on_key(
        &mut self,
        key: Key,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> MenuOutcome {
        if key == Key::Named(NamedKey::Escape) {
            return MenuOutcome::Dismissed;
        }
        let bounds = self.bounds(viewport, scale, theme);
        let action = self.menu.on_key(key, bounds, scale, theme, damage);
        Self::outcome(action, damage)
    }

    /// What the shared control's `action` and the pixels it reported into
    /// `damage` amount to for the caller.
    ///
    /// The shared control reports the rows it redraws, and reports nothing at
    /// all for an event that leaves every drawn field where it was — that is
    /// the difference between [`MenuOutcome::Changed`] and
    /// [`MenuOutcome::Ignored`], so a caller that repaints on `Changed`
    /// repaints exactly when something moved. The sink covers one round of
    /// input, so an event that changes nothing after one that did keeps the
    /// round's answer.
    fn outcome(action: Option<MenuAction>, damage: &Region) -> MenuOutcome {
        match action {
            Some(MenuAction::Activated { index }) => Command::ALL
                .get(index)
                .copied()
                .map_or(MenuOutcome::Dismissed, MenuOutcome::Chose),
            // No row owns a submenu, so a submenu request cannot arise; a
            // dismissal closes the menu.
            Some(MenuAction::OpenSubmenu { .. } | MenuAction::Dismissed) => MenuOutcome::Dismissed,
            None if damage.is_empty() => MenuOutcome::Ignored,
            None => MenuOutcome::Changed,
        }
    }
}

#[cfg(test)]
#[path = "menu_tests.rs"]
mod tests;
