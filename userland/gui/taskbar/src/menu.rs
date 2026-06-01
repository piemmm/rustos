//! The start menu and its entries.
//!
//! The start-menu button sits at the leading end of the taskbar. The menu
//! is deliberately **not** an application launcher at this stage: it is
//! populated only with the session controls (log out, lock, shut down,
//! restart). It is shaped so launcher entries can be added later — as a new
//! [`MenuAction`] variant carried by ordinary [`MenuEntry`] values — without
//! changing the public list/activate interface (`PLAN.md` Stage 7).

use alloc::vec::Vec;

/// A session-control action offered by the start menu.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SessionControl {
    /// End the current user's session.
    LogOut,
    /// Lock the screen, keeping the session running.
    Lock,
    /// Power the machine off.
    ShutDown,
    /// Reboot the machine.
    Restart,
}

impl SessionControl {
    /// The fixed display label for the control.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::LogOut => "Log Out",
            Self::Lock => "Lock",
            Self::ShutDown => "Shut Down",
            Self::Restart => "Restart",
        }
    }

    /// The session controls, in their fixed menu order.
    #[must_use]
    pub const fn all() -> [Self; 4] {
        [Self::LogOut, Self::Lock, Self::ShutDown, Self::Restart]
    }
}

/// What activating a [`MenuEntry`] does.
///
/// Today every entry is a [`SessionControl`]. Launcher entries are a future
/// increment: they arrive as a new variant here, so the
/// [`StartMenu::entries`] / [`StartMenu::activate`] interface does not change
/// when they land (`AGENTS.md` §2.4 — extend, do not creep).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MenuAction {
    /// Invoke a session control.
    Session(SessionControl),
}

/// A stable identifier for a [`MenuEntry`] within a menu.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct MenuEntryId(pub u32);

/// One row of the start menu: a stable id and the action it triggers.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MenuEntry {
    /// The entry's stable id, unique within its menu.
    pub id: MenuEntryId,
    /// The action performed when the entry is activated.
    pub action: MenuAction,
}

impl MenuEntry {
    /// The entry's display label.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self.action {
            MenuAction::Session(control) => control.label(),
        }
    }
}

/// The start menu: an open/closed toggle and an ordered list of entries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartMenu {
    open: bool,
    entries: Vec<MenuEntry>,
}

impl StartMenu {
    /// A closed menu populated with the session controls in their fixed
    /// order, assigned stable ids `1..=4`.
    #[must_use]
    pub fn with_session_controls() -> Self {
        let mut entries = Vec::new();
        for (index, control) in SessionControl::all().into_iter().enumerate() {
            let ordinal = u32::try_from(index).unwrap_or(u32::MAX);
            entries.push(MenuEntry {
                id: MenuEntryId(ordinal.saturating_add(1)),
                action: MenuAction::Session(control),
            });
        }
        Self {
            open: false,
            entries,
        }
    }

    /// `true` when the menu is showing.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// Toggle the menu open/closed (what the start button does) and return
    /// the new state.
    pub fn toggle(&mut self) -> bool {
        self.open = !self.open;
        self.open
    }

    /// Close the menu. Idempotent.
    pub fn close(&mut self) {
        self.open = false;
    }

    /// The menu's entries in display order.
    #[must_use]
    pub fn entries(&self) -> &[MenuEntry] {
        &self.entries
    }

    /// The number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when the menu has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Activate the entry with `id`, closing the menu, and return its
    /// action. An unknown id changes nothing and returns `None` (fail
    /// closed, `AGENTS.md` §5.4 / §2.9).
    pub fn activate(&mut self, id: MenuEntryId) -> Option<MenuAction> {
        let action = self.entries.iter().find(|e| e.id == id)?.action;
        self.open = false;
        Some(action)
    }
}

impl Default for StartMenu {
    fn default() -> Self {
        Self::with_session_controls()
    }
}
